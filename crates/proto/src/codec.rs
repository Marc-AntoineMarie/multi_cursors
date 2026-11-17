//! Cadrage des messages sur un flux TCP : longueur sur 4 octets (gros-boutiste) puis `postcard`.

use crate::{Msg, MAX_FRAME};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("erreur d'entrée/sortie : {0}")]
    Io(#[from] std::io::Error),
    #[error("message illisible : {0}")]
    Decode(postcard::Error),
    #[error("message de {0} octets, au-delà de la limite de {MAX_FRAME}")]
    TooLarge(usize),
    #[error("le pair a fermé la connexion")]
    Closed,
}

pub fn encode(msg: &Msg) -> Result<Vec<u8>, CodecError> {
    let payload = postcard::to_stdvec(msg).map_err(CodecError::Decode)?;
    if payload.len() > MAX_FRAME {
        return Err(CodecError::TooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode(payload: &[u8]) -> Result<Msg, CodecError> {
    postcard::from_bytes(payload).map_err(CodecError::Decode)
}

#[cfg(feature = "codec")]
mod asynchrone {
    use super::{decode, encode, CodecError};
    use crate::{Msg, MAX_FRAME};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn write_msg<W>(writer: &mut W, msg: &Msg) -> Result<(), CodecError>
    where
        W: AsyncWrite + Unpin,
    {
        let frame = encode(msg)?;
        writer.write_all(&frame).await?;
        // Les messages pointeur sont minuscules et doivent partir tout de suite : pas de
        // regroupement côté application, et `TCP_NODELAY` côté socket.
        writer.flush().await?;
        Ok(())
    }

    pub async fn read_msg<R>(reader: &mut R) -> Result<Msg, CodecError>
    where
        R: AsyncRead + Unpin,
    {
        let mut len = [0u8; 4];
        match reader.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(CodecError::Closed)
            }
            Err(e) => return Err(CodecError::Io(e)),
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err(CodecError::TooLarge(len));
        }
        let mut payload = vec![0u8; len];
        match reader.read_exact(&mut payload).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(CodecError::Closed)
            }
            Err(e) => return Err(CodecError::Io(e)),
        }
        decode(&payload)
    }
}

#[cfg(feature = "codec")]
pub use asynchrone::{read_msg, write_msg};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Button;

    #[test]
    fn cadrage_aller_retour() {
        let msg = Msg::Button {
            button: Button::Left,
            pressed: true,
        };
        let frame = encode(&msg).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode(&frame[4..]).unwrap(), msg);
    }

    #[cfg(feature = "codec")]
    #[tokio::test]
    async fn flux_de_plusieurs_messages() {
        let msgs = vec![
            Msg::Enter { x: 0, y: 500 },
            Msg::MoveTo { x: 10, y: 500 },
            Msg::Leave,
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            buf.extend_from_slice(&encode(m).unwrap());
        }
        // `&[u8]` implémente `AsyncRead` : pas besoin de vraie socket pour tester le cadrage.
        let mut stream: &[u8] = &buf;
        for expected in &msgs {
            assert_eq!(&read_msg(&mut stream).await.unwrap(), expected);
        }
        assert!(matches!(
            read_msg(&mut stream).await,
            Err(CodecError::Closed)
        ));
    }
}

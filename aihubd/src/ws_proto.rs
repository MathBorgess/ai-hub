//! WebSocket mínimo (RFC 6455) escrito à mão sobre `tokio::io`, sem `futures-util`/`sha1`
//! externos: nenhum dos dois está declarado nas dependências de `aihubd` (só chegam
//! transitivamente via `tokio-tungstenite`/`tungstenite`), e a sessão 04 não pode editar
//! `Cargo.toml`/`Cargo.lock` (contrato desta fatia). SHA-1 só é necessário para o cálculo do
//! `Sec-WebSocket-Accept` do handshake — um algoritmo público de tamanho fixo, reimplementado
//! aqui inteiramente (ver `sha1` abaixo) em vez de puxar uma dependência nova.
//!
//! Cobre exatamente o que o canal de controle e o canal de PTY precisam: handshake HTTP
//! Upgrade, frames de texto/binário completos (sem fragmentação — nenhum lado nosso fragmenta
//! uma mensagem), ping/pong e close. Não é uma implementação genérica de WebSocket.
use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// SHA-1 puro, usado apenas para `Sec-WebSocket-Accept` (RFC 6455 §1.3). Implementação de
/// referência do algoritmo público, sem estado externo.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A827999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
            } else {
                (b ^ c ^ d, 0xCA62C1D6)
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Reaproveita `Base64Bytes` (dependência já existente de `aihub-core`) para codificar em
/// Base64 padrão sem depender diretamente do crate `base64`.
fn base64_encode(bytes: &[u8]) -> String {
    let encoded = aihub_core::Base64Bytes::new(bytes.to_vec());
    let json = serde_json::to_string(&encoded).unwrap_or_default();
    json.trim_matches('"').to_string()
}

/// Lê e responde o handshake HTTP Upgrade de um cliente WebSocket. Falha se a requisição não
/// tiver `Sec-WebSocket-Key`.
pub async fn accept_handshake<S>(stream: &mut S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed during websocket handshake");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 16 * 1024 {
            bail!("websocket handshake request too large");
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("sec-websocket-key") {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("missing Sec-WebSocket-Key header"))?;

    let mut accept_src = key.into_bytes();
    accept_src.extend_from_slice(GUID.as_bytes());
    let accept = base64_encode(&sha1(&accept_src));

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// Lê exatamente um frame completo (sem fragmentação — nenhum peer nosso fragmenta). Frames
/// de cliente para servidor devem estar mascarados por spec; frames não mascarados são
/// aceitos por leniência (nunca emitidos por nós), nunca exigidos.
pub async fn read_message<S>(stream: &mut S) -> Result<WsMessage>
where
    S: AsyncRead + Unpin,
{
    let header = stream.read_u16().await?;
    let fin = header & 0x8000 != 0;
    let opcode = ((header >> 8) & 0x0F) as u8;
    let masked = header & 0x0080 != 0;
    let len7 = (header & 0x007F) as u8;
    if !fin {
        bail!("fragmented websocket messages are not supported");
    }
    let len: u64 = match len7 {
        126 => stream.read_u16().await? as u64,
        127 => stream.read_u64().await?,
        n => n as u64,
    };
    const MAX_FRAME: u64 = 32 * 1024 * 1024;
    if len > MAX_FRAME {
        bail!("websocket frame too large: {len} bytes");
    }
    let mask_key = if masked {
        let mut k = [0u8; 4];
        stream.read_exact(&mut k).await?;
        Some(k)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    if let Some(k) = mask_key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= k[i % 4];
        }
    }
    match opcode {
        OP_TEXT => Ok(WsMessage::Text(String::from_utf8(payload)?)),
        OP_BINARY => Ok(WsMessage::Binary(payload)),
        OP_CLOSE => Ok(WsMessage::Close),
        OP_PING => Ok(WsMessage::Ping(payload)),
        OP_PONG => Ok(WsMessage::Pong(payload)),
        other => bail!("unsupported websocket opcode: {other}"),
    }
}

/// Escreve um frame não mascarado (servidor -> cliente nunca mascara, RFC 6455 §5.1).
pub async fn write_message<S>(stream: &mut S, message: &WsMessage) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let (opcode, payload): (u8, &[u8]) = match message {
        WsMessage::Text(s) => (OP_TEXT, s.as_bytes()),
        WsMessage::Binary(b) => (OP_BINARY, b.as_slice()),
        WsMessage::Ping(b) => (OP_PING, b.as_slice()),
        WsMessage::Pong(b) => (OP_PONG, b.as_slice()),
        WsMessage::Close => (OP_CLOSE, &[]),
    };
    let mut out = Vec::with_capacity(10 + payload.len());
    out.push(0x80 | opcode); // FIN=1
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    stream.write_all(&out).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(
            sha1(b""),
            [
                0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
                0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09
            ]
        );
        assert_eq!(
            sha1(b"abc"),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }

    #[test]
    fn accept_key_matches_rfc6455_example() {
        // Example straight from RFC 6455 §1.3.
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let mut src = key.as_bytes().to_vec();
        src.extend_from_slice(GUID.as_bytes());
        let accept = base64_encode(&sha1(&src));
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[tokio::test]
    async fn write_then_read_binary_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let payload = vec![1u8, 2, 3, 4, 250];
        write_message(&mut a, &WsMessage::Binary(payload.clone()))
            .await
            .unwrap();
        let msg = read_message(&mut b).await.unwrap();
        assert_eq!(msg, WsMessage::Binary(payload));
    }

    #[tokio::test]
    async fn write_then_read_large_binary_uses_extended_length() {
        let (mut a, mut b) = tokio::io::duplex(4 * 1024 * 1024);
        let payload = vec![9u8; 200_000];
        write_message(&mut a, &WsMessage::Binary(payload.clone()))
            .await
            .unwrap();
        let msg = read_message(&mut b).await.unwrap();
        assert_eq!(msg, WsMessage::Binary(payload));
    }

    #[tokio::test]
    async fn reads_masked_client_frame() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let payload = b"hello".to_vec();
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        let mut masked_payload = payload.clone();
        for (i, byte) in masked_payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
        let mut frame = vec![0x80 | OP_TEXT, 0x80 | (payload.len() as u8)];
        frame.extend_from_slice(&mask);
        frame.extend_from_slice(&masked_payload);
        a.write_all(&frame).await.unwrap();
        let msg = read_message(&mut b).await.unwrap();
        assert_eq!(msg, WsMessage::Text("hello".into()));
    }

    #[tokio::test]
    async fn handshake_computes_correct_accept_header() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let request = "GET /aihub HTTP/1.1\r\n\
             Host: 127.0.0.1:9920\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n";
        client.write_all(request.as_bytes()).await.unwrap();
        accept_handshake(&mut server).await.unwrap();
        let mut resp = vec![0u8; 4096];
        let n = client.read(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp[..n]);
        assert!(resp.contains("101 Switching Protocols"));
        assert!(resp.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    }
}

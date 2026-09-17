//! Ring buffer de retomada por offset (01-transporte-e-sessao.md §2.2, §3; ADR §2.2).
//!
//! Cada sessão mantém exatamente um destes, com teto fixo de 2 MiB em RAM. Todo byte de
//! saída do PTY recebe um `stream_offset: u64` monotônico contínuo, começando em 0. O
//! descarte é sempre do mais antigo (`head_offset` avança), nunca persiste em disco.
use std::collections::VecDeque;

/// Teto por sessão (ADR §2.2, §7): 2 MiB × N sessões é o custo de RAM aceito pelo dono.
pub const RING_BUFFER_CAP: usize = 2 * 1024 * 1024;

/// Ring buffer sequenciado por offset global. `head_offset` é o menor offset ainda retido,
/// `tail_offset` é o próximo offset a ser gravado — o intervalo retido é `[head, tail)`.
pub struct RingBuffer {
    cap: usize,
    data: VecDeque<u8>,
    tail_offset: u64,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            data: VecDeque::new(),
            tail_offset: 0,
        }
    }

    pub fn head_offset(&self) -> u64 {
        self.tail_offset - self.data.len() as u64
    }

    pub fn tail_offset(&self) -> u64 {
        self.tail_offset
    }

    /// Grava `bytes`, descartando o mais antigo além do teto. Retorna o `stream_offset` do
    /// primeiro byte de `bytes` (o offset que a mensagem correspondente deve carregar).
    pub fn push(&mut self, bytes: &[u8]) -> u64 {
        let start = self.tail_offset;
        self.data.extend(bytes.iter().copied());
        self.tail_offset += bytes.len() as u64;
        let excess = self.data.len().saturating_sub(self.cap);
        if excess > 0 {
            self.data.drain(..excess);
        }
        start
    }

    /// Cópia completa do que está retido agora (para `gap_detected: true`, Caso B).
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.iter().copied().collect()
    }

    /// Delta `[last_seen_offset, tail_offset)`, ou `None` se `last_seen_offset` já caiu fora
    /// da janela retida (`gap_detected`, Caso B do design). Um offset igual a `tail_offset`
    /// é válido e produz um delta vazio (cliente já está em dia).
    pub fn delta_since(&self, last_seen_offset: u64) -> Option<Vec<u8>> {
        let head = self.head_offset();
        if last_seen_offset < head || last_seen_offset > self.tail_offset {
            return None;
        }
        let skip = (last_seen_offset - head) as usize;
        Some(self.data.iter().skip(skip).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_returns_starting_offset_and_advances_tail() {
        let mut ring = RingBuffer::new(16);
        assert_eq!(ring.push(b"abc"), 0);
        assert_eq!(ring.tail_offset(), 3);
        assert_eq!(ring.push(b"de"), 3);
        assert_eq!(ring.tail_offset(), 5);
    }

    #[test]
    fn discards_oldest_past_cap() {
        let mut ring = RingBuffer::new(4);
        ring.push(b"abcdef"); // 6 bytes > cap 4: keeps last 4 ("cdef")
        assert_eq!(ring.head_offset(), 2);
        assert_eq!(ring.tail_offset(), 6);
        assert_eq!(ring.snapshot(), b"cdef".to_vec());
    }

    #[test]
    fn delta_since_within_window() {
        let mut ring = RingBuffer::new(1024);
        ring.push(b"hello ");
        ring.push(b"world");
        assert_eq!(ring.delta_since(6), Some(b"world".to_vec()));
        assert_eq!(ring.delta_since(0), Some(b"hello world".to_vec()));
        assert_eq!(ring.delta_since(11), Some(vec![]));
    }

    #[test]
    fn delta_since_outside_window_is_gap() {
        let mut ring = RingBuffer::new(4);
        ring.push(b"abcdef"); // head_offset now 2
        assert_eq!(ring.delta_since(0), None);
        assert_eq!(ring.delta_since(7), None); // beyond tail is also invalid
        assert_eq!(ring.delta_since(2), Some(b"cdef".to_vec()));
    }

    #[test]
    fn two_mib_cap_holds_exactly_two_mib() {
        let mut ring = RingBuffer::new(RING_BUFFER_CAP);
        let chunk = vec![7u8; RING_BUFFER_CAP + 1];
        ring.push(&chunk);
        assert_eq!(ring.snapshot().len(), RING_BUFFER_CAP);
        assert_eq!(ring.head_offset(), 1);
    }
}

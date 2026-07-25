//! Piece and block scheduling.
//!
//! The picker owns the truth about which pieces are verified, which blocks
//! are in flight and to whom, and how rare each piece is. Peer tasks claim
//! batches of blocks, report received data, and release claims when their
//! connection dies.
//!
//! Strategy: finish already-started pieces first, then open the rarest piece
//! the peer can provide (random among ties). When every missing block is
//! claimed, endgame mode allows duplicate claims so a few slow peers cannot
//! stall the last percent.

use std::collections::HashMap;

use rand::RngExt;

/// Blocks are requested in 16 KiB chunks, as in the original implementation.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// One block request: a piece index, an offset inside it, and a length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub piece: u32,
    pub begin: u32,
    pub length: u32,
}

/// Result of feeding a received block into the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// New data; `piece_complete` is set when every block of the piece is in.
    Accepted { piece_complete: bool },
    /// We already had this block (common in endgame); the bytes are wasted.
    Duplicate,
    /// Block does not belong to any piece we are missing.
    Ignored,
}

struct PartialPiece {
    /// Block index -> received?
    received: Vec<bool>,
    /// Block index -> peer key that currently has it claimed.
    claimed: Vec<Option<u64>>,
    received_count: u32,
    /// Peers that contributed blocks, for penalising on hash failure.
    contributors: Vec<u64>,
}

pub struct PiecePicker {
    piece_count: u32,
    piece_length: u64,
    total_length: u64,
    /// Verified pieces.
    have: Vec<bool>,
    have_count: u32,
    /// Pieces fully downloaded, awaiting hash verification.
    pending_verify: Vec<bool>,
    /// How many connected peers have each piece.
    availability: Vec<u32>,
    partials: HashMap<u32, PartialPiece>,
    endgame: bool,
}

impl PiecePicker {
    pub fn new(piece_count: u32, piece_length: u64, total_length: u64, have: Vec<bool>) -> Self {
        assert_eq!(have.len(), piece_count as usize);
        let have_count = have.iter().filter(|&&h| h).count() as u32;
        PiecePicker {
            piece_count,
            piece_length,
            total_length,
            have,
            have_count,
            pending_verify: vec![false; piece_count as usize],
            availability: vec![0; piece_count as usize],
            partials: HashMap::new(),
            endgame: false,
        }
    }

    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    pub fn have_count(&self) -> u32 {
        self.have_count
    }

    pub fn is_complete(&self) -> bool {
        self.have_count == self.piece_count
    }

    pub fn in_endgame(&self) -> bool {
        self.endgame
    }

    pub fn has_piece(&self, piece: u32) -> bool {
        self.have.get(piece as usize).copied().unwrap_or(false)
    }

    /// Verified payload bytes, the number shown as download progress.
    pub fn verified_bytes(&self) -> u64 {
        (0..self.piece_count)
            .filter(|&p| self.have[p as usize])
            .map(|p| self.piece_size(p))
            .sum()
    }

    pub fn piece_size(&self, piece: u32) -> u64 {
        let start = u64::from(piece) * self.piece_length;
        (self.total_length - start).min(self.piece_length)
    }

    fn block_count(&self, piece: u32) -> u32 {
        self.piece_size(piece).div_ceil(u64::from(BLOCK_SIZE)) as u32
    }

    fn block_length(&self, piece: u32, block: u32) -> u32 {
        let piece_size = self.piece_size(piece);
        let begin = u64::from(block) * u64::from(BLOCK_SIZE);
        (piece_size - begin).min(u64::from(BLOCK_SIZE)) as u32
    }

    /// Our bitfield in wire format (BEP 3: high bit of byte 0 is piece 0).
    pub fn bitfield_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; (self.piece_count as usize).div_ceil(8)];
        for piece in 0..self.piece_count as usize {
            if self.have[piece] {
                bytes[piece / 8] |= 0x80 >> (piece % 8);
            }
        }
        bytes
    }

    /// Register a peer's bitfield for availability tracking.
    pub fn peer_has_bitfield(&mut self, bits: &PeerBitfield) {
        for piece in 0..self.piece_count {
            if bits.has(piece) {
                self.availability[piece as usize] += 1;
            }
        }
    }

    pub fn peer_has_piece(&mut self, piece: u32) {
        if let Some(count) = self.availability.get_mut(piece as usize) {
            *count += 1;
        }
    }

    /// Remove a departing peer's contribution to availability and claims.
    pub fn peer_gone(&mut self, peer_key: u64, bits: &PeerBitfield) {
        for piece in 0..self.piece_count {
            if bits.has(piece) {
                let count = &mut self.availability[piece as usize];
                *count = count.saturating_sub(1);
            }
        }
        self.release_claims(peer_key);
    }

    /// Return every block claimed by `peer_key` to the unclaimed pool.
    pub fn release_claims(&mut self, peer_key: u64) {
        for partial in self.partials.values_mut() {
            for claim in partial.claimed.iter_mut() {
                if *claim == Some(peer_key) {
                    *claim = None;
                }
            }
        }
    }

    /// Would we want anything this peer has?
    pub fn is_interesting(&self, bits: &PeerBitfield) -> bool {
        (0..self.piece_count)
            .any(|p| bits.has(p) && !self.have[p as usize] && !self.pending_verify[p as usize])
    }

    /// Claim up to `max_blocks` blocks that `peer` can provide.
    pub fn pick_blocks(
        &mut self,
        peer_key: u64,
        bits: &PeerBitfield,
        max_blocks: usize,
    ) -> Vec<BlockRef> {
        let mut picked = Vec::with_capacity(max_blocks);

        // 1. Unclaimed blocks of pieces already in progress.
        let mut in_progress: Vec<u32> = self.partials.keys().copied().collect();
        in_progress.sort_unstable();
        for piece in in_progress {
            if picked.len() >= max_blocks {
                break;
            }
            if bits.has(piece) {
                self.claim_from_partial(piece, peer_key, max_blocks, &mut picked);
            }
        }

        // 2. Open new pieces, rarest first, while the peer has capacity.
        while picked.len() < max_blocks {
            let Some(piece) = self.choose_new_piece(bits) else {
                break;
            };
            self.partials.insert(
                piece,
                PartialPiece {
                    received: vec![false; self.block_count(piece) as usize],
                    claimed: vec![None; self.block_count(piece) as usize],
                    received_count: 0,
                    contributors: Vec::new(),
                },
            );
            self.claim_from_partial(piece, peer_key, max_blocks, &mut picked);
        }

        // 3. Endgame: duplicate-claim blocks other peers are slow to deliver.
        if picked.is_empty() && self.should_enter_endgame() {
            self.endgame = true;
            let mut pieces: Vec<u32> = self.partials.keys().copied().collect();
            pieces.sort_unstable();
            'outer: for piece in pieces {
                if !bits.has(piece) {
                    continue;
                }
                let block_lengths: Vec<u32> = (0..self.block_count(piece))
                    .map(|b| self.block_length(piece, b))
                    .collect();
                let Some(partial) = self.partials.get(&piece) else {
                    continue;
                };
                for (block, &length) in block_lengths.iter().enumerate() {
                    if !partial.received[block] && partial.claimed[block] != Some(peer_key) {
                        picked.push(BlockRef {
                            piece,
                            begin: block as u32 * BLOCK_SIZE,
                            length,
                        });
                        if picked.len() >= max_blocks {
                            break 'outer;
                        }
                    }
                }
            }
        }

        picked
    }

    fn claim_from_partial(
        &mut self,
        piece: u32,
        peer_key: u64,
        max_blocks: usize,
        picked: &mut Vec<BlockRef>,
    ) {
        let block_lengths: Vec<u32> = (0..self.block_count(piece))
            .map(|b| self.block_length(piece, b))
            .collect();
        let Some(partial) = self.partials.get_mut(&piece) else {
            return;
        };
        for (block, &length) in block_lengths.iter().enumerate() {
            if picked.len() >= max_blocks {
                break;
            }
            if !partial.received[block] && partial.claimed[block].is_none() {
                partial.claimed[block] = Some(peer_key);
                picked.push(BlockRef {
                    piece,
                    begin: block as u32 * BLOCK_SIZE,
                    length,
                });
            }
        }
    }

    /// Rarest missing piece the peer has, random among equally rare ones.
    fn choose_new_piece(&self, bits: &PeerBitfield) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None; // (availability, piece)
        let mut ties = 0u32;
        let mut rng = rand::rng();

        for piece in 0..self.piece_count {
            let index = piece as usize;
            if self.have[index]
                || self.pending_verify[index]
                || self.partials.contains_key(&piece)
                || !bits.has(piece)
            {
                continue;
            }
            let availability = self.availability[index];
            match best {
                None => {
                    best = Some((availability, piece));
                    ties = 1;
                }
                Some((best_avail, _)) if availability < best_avail => {
                    best = Some((availability, piece));
                    ties = 1;
                }
                Some((best_avail, _)) if availability == best_avail => {
                    // Reservoir sampling keeps the choice uniform among ties.
                    ties += 1;
                    if rng.random_range(0..ties) == 0 {
                        best = Some((availability, piece));
                    }
                }
                _ => {}
            }
        }
        best.map(|(_, piece)| piece)
    }

    fn should_enter_endgame(&self) -> bool {
        if self.is_complete() {
            return false;
        }
        // Every missing piece is already being worked on, and every block of
        // those pieces is claimed or received.
        for piece in 0..self.piece_count {
            let index = piece as usize;
            if self.have[index] || self.pending_verify[index] {
                continue;
            }
            match self.partials.get(&piece) {
                None => return false,
                Some(partial) => {
                    let all_covered = partial
                        .received
                        .iter()
                        .zip(&partial.claimed)
                        .all(|(received, claimed)| *received || claimed.is_some());
                    if !all_covered {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Feed one received block. `peer_key` is recorded for hash-failure blame.
    pub fn block_received(
        &mut self,
        peer_key: u64,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> Received {
        if piece >= self.piece_count || !begin.is_multiple_of(BLOCK_SIZE) {
            return Received::Ignored;
        }
        let block = (begin / BLOCK_SIZE) as usize;
        if block as u32 >= self.block_count(piece)
            || length != self.block_length(piece, block as u32)
        {
            return Received::Ignored;
        }
        let Some(partial) = self.partials.get_mut(&piece) else {
            return Received::Ignored;
        };
        if partial.received[block] {
            return Received::Duplicate;
        }
        partial.received[block] = true;
        partial.claimed[block] = None;
        partial.received_count += 1;
        if !partial.contributors.contains(&peer_key) {
            partial.contributors.push(peer_key);
        }

        let complete = partial.received_count as usize == partial.received.len();
        if complete {
            self.pending_verify[piece as usize] = true;
        }
        Received::Accepted {
            piece_complete: complete,
        }
    }

    /// Mark a fully-downloaded piece as verified.
    pub fn piece_verified(&mut self, piece: u32) {
        self.partials.remove(&piece);
        self.pending_verify[piece as usize] = false;
        if !self.have[piece as usize] {
            self.have[piece as usize] = true;
            self.have_count += 1;
        }
        if self.is_complete() {
            self.endgame = false;
        }
    }

    /// A piece failed its hash check: forget it so it gets downloaded again,
    /// and report which peers contributed to it.
    pub fn piece_failed(&mut self, piece: u32) -> Vec<u64> {
        self.pending_verify[piece as usize] = false;
        self.partials
            .remove(&piece)
            .map(|p| p.contributors)
            .unwrap_or_default()
    }

    /// Blocks still missing or unverified, for progress displays.
    pub fn remaining_bytes(&self) -> u64 {
        self.total_length - self.verified_bytes()
    }

    pub fn have_vec(&self) -> Vec<bool> {
        self.have.clone()
    }
}

/// A remote peer's piece set, updated from `bitfield` and `have` messages.
#[derive(Clone, Debug, Default)]
pub struct PeerBitfield {
    bits: Vec<u8>,
    piece_count: u32,
}

impl PeerBitfield {
    pub fn new(piece_count: u32) -> Self {
        PeerBitfield {
            bits: vec![0; (piece_count as usize).div_ceil(8)],
            piece_count,
        }
    }

    /// Replace contents from a wire-format bitfield message.
    pub fn set_from_bytes(&mut self, bytes: &[u8]) {
        for (i, byte) in self.bits.iter_mut().enumerate() {
            *byte = bytes.get(i).copied().unwrap_or(0);
        }
    }

    pub fn set(&mut self, piece: u32) {
        if piece < self.piece_count {
            self.bits[(piece / 8) as usize] |= 0x80 >> (piece % 8);
        }
    }

    pub fn has(&self, piece: u32) -> bool {
        piece < self.piece_count && self.bits[(piece / 8) as usize] & (0x80 >> (piece % 8)) != 0
    }

    pub fn count(&self) -> u32 {
        (0..self.piece_count).filter(|&p| self.has(p)).count() as u32
    }

    /// True when the peer has every piece (a seeder).
    pub fn is_complete(&self) -> bool {
        self.count() == self.piece_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_bitfield(piece_count: u32) -> PeerBitfield {
        let mut bits = PeerBitfield::new(piece_count);
        for p in 0..piece_count {
            bits.set(p);
        }
        bits
    }

    /// 3 pieces of 32 KiB except the last (10 KiB): 2+2+1 = 5 blocks.
    fn picker() -> PiecePicker {
        PiecePicker::new(3, 32 * 1024, 32 * 1024 * 2 + 10 * 1024, vec![false; 3])
    }

    #[test]
    fn picks_all_blocks_and_completes() {
        let mut picker = PiecePicker::new(2, 32 * 1024, 32 * 1024 + 100, vec![false, false]);
        let bits = full_bitfield(2);
        picker.peer_has_bitfield(&bits);

        let blocks = picker.pick_blocks(1, &bits, 16);
        // Piece 0: 2 blocks of 16 KiB. Piece 1: one 100-byte block.
        assert_eq!(blocks.len(), 3);
        assert!(blocks.contains(&BlockRef {
            piece: 1,
            begin: 0,
            length: 100
        }));

        for block in &blocks {
            let result = picker.block_received(1, block.piece, block.begin, block.length);
            assert!(matches!(result, Received::Accepted { .. }));
        }
        picker.piece_verified(0);
        picker.piece_verified(1);
        assert!(picker.is_complete());
        assert_eq!(picker.verified_bytes(), 32 * 1024 + 100);
    }

    #[test]
    fn does_not_double_claim() {
        let mut picker = picker();
        let bits = full_bitfield(3);
        picker.peer_has_bitfield(&bits);
        picker.peer_has_bitfield(&bits);

        // 5 blocks total; peer 1 takes two, peer 2 must get the other three.
        let first = picker.pick_blocks(1, &bits, 2);
        let second = picker.pick_blocks(2, &bits, 100);
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 3);
        assert!(first.iter().all(|b| !second.contains(b)));
        assert!(!picker.in_endgame());
    }

    #[test]
    fn released_claims_can_be_repicked() {
        let mut picker = picker();
        let bits = full_bitfield(3);
        let taken = picker.pick_blocks(1, &bits, 2);
        assert_eq!(taken.len(), 2);
        picker.release_claims(1);
        let retaken = picker.pick_blocks(2, &bits, 100);
        assert!(taken.iter().all(|b| retaken.contains(b)));
    }

    #[test]
    fn rarest_piece_is_opened_first() {
        let mut picker = picker();
        // Piece 2 is on two peers, pieces 0 and 1 on three.
        let mut rare = PeerBitfield::new(3);
        rare.set(0);
        rare.set(1);
        picker.peer_has_bitfield(&full_bitfield(3));
        picker.peer_has_bitfield(&full_bitfield(3));
        picker.peer_has_bitfield(&rare);

        let blocks = picker.pick_blocks(1, &full_bitfield(3), 1);
        assert_eq!(blocks[0].piece, 2, "piece 2 is rarest");
    }

    #[test]
    fn endgame_allows_duplicate_claims() {
        let mut picker = picker();
        let bits = full_bitfield(3);
        picker.peer_has_bitfield(&bits);

        let all = picker.pick_blocks(1, &bits, 100);
        assert_eq!(all.len(), 5);
        // Peer 2 arrives; everything is claimed by peer 1 -> endgame kicks in.
        let dup = picker.pick_blocks(2, &bits, 2);
        assert_eq!(dup.len(), 2);
        assert!(picker.in_endgame());
        // The duplicate delivers first; the original is then a duplicate.
        assert!(matches!(
            picker.block_received(2, dup[0].piece, dup[0].begin, dup[0].length),
            Received::Accepted { .. }
        ));
        assert_eq!(
            picker.block_received(1, dup[0].piece, dup[0].begin, dup[0].length),
            Received::Duplicate
        );
    }

    #[test]
    fn hash_failure_requeues_piece_and_names_contributors() {
        let mut picker = PiecePicker::new(1, 16 * 1024, 16 * 1024, vec![false]);
        let bits = full_bitfield(1);
        let blocks = picker.pick_blocks(7, &bits, 10);
        assert_eq!(blocks.len(), 1);
        let result = picker.block_received(7, 0, 0, 16 * 1024);
        assert_eq!(
            result,
            Received::Accepted {
                piece_complete: true
            }
        );

        let blamed = picker.piece_failed(0);
        assert_eq!(blamed, vec![7]);
        // The piece can be picked again.
        let again = picker.pick_blocks(8, &bits, 10);
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn bitfield_wire_format() {
        let mut picker = PiecePicker::new(9, 1, 9, vec![false; 9]);
        picker.piece_verified(0);
        picker.piece_verified(8);
        assert_eq!(picker.bitfield_bytes(), vec![0b1000_0000, 0b1000_0000]);
    }

    #[test]
    fn interest_tracking() {
        let mut picker = PiecePicker::new(2, 4, 8, vec![true, false]);
        let mut bits = PeerBitfield::new(2);
        bits.set(0);
        assert!(!picker.is_interesting(&bits), "we already have piece 0");
        bits.set(1);
        assert!(picker.is_interesting(&bits));
        picker.piece_verified(1);
        assert!(!picker.is_interesting(&bits));
    }
}

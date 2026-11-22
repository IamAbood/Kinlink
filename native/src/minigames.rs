//! Turn-based mini games with a friend: tic-tac-toe and Connect Four.
//! A game is just its move list; each move sends the whole list (`Sig::Game`), so a lost or
//! repeated message can't break it: the longer valid list wins. The inviter moves first.
use crate::crypto::Pub;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    TicTacToe,
    ConnectFour,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::TicTacToe, Kind::ConnectFour];

    pub fn from_u8(k: u8) -> Option<Kind> {
        Kind::ALL.get(k as usize).copied()
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::TicTacToe => "Tic-tac-toe",
            Kind::ConnectFour => "Connect Four",
        }
    }
    /// (columns, rows)
    pub fn size(self) -> (usize, usize) {
        match self {
            Kind::TicTacToe => (3, 3),
            Kind::ConnectFour => (7, 6),
        }
    }
    fn line(self) -> usize {
        match self {
            Kind::TicTacToe => 3,
            Kind::ConnectFour => 4,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Playing,
    /// 1 = the player who started, 2 = the other one.
    Won(u8),
    Draw,
}

/// Cells row by row from the top (0 empty, 1 first player, 2 second). None if a move is illegal.
/// Tic-tac-toe moves are cell indexes; Connect Four moves are columns (the piece drops down).
pub fn board(kind: Kind, moves: &[u8]) -> Option<Vec<u8>> {
    let (w, h) = kind.size();
    let mut b = vec![0u8; w * h];
    for (i, &m) in moves.iter().enumerate() {
        if outcome(kind, &b) != Outcome::Playing {
            return None; // moves after the game ended
        }
        let cell = match kind {
            Kind::TicTacToe => Some(m as usize).filter(|&c| c < b.len() && b[c] == 0)?,
            Kind::ConnectFour => (0..h).rev().map(|r| r * w + m as usize).find(|&c| (m as usize) < w && b[c] == 0)?,
        };
        b[cell] = 1 + (i % 2) as u8;
    }
    Some(b)
}

pub fn outcome(kind: Kind, b: &[u8]) -> Outcome {
    let (w, h) = kind.size();
    let n = kind.line() as i32;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let p = b[(y * w as i32 + x) as usize];
            if p == 0 {
                continue;
            }
            for (dx, dy) in [(1, 0), (0, 1), (1, 1), (1, -1)] {
                let all = (0..n).all(|k| {
                    let (cx, cy) = (x + dx * k, y + dy * k);
                    cx >= 0 && cy >= 0 && cx < w as i32 && cy < h as i32 && b[(cy * w as i32 + cx) as usize] == p
                });
                if all {
                    return Outcome::Won(p);
                }
            }
        }
    }
    if b.iter().all(|&c| c != 0) { Outcome::Draw } else { Outcome::Playing }
}

/// The game on screen.
pub struct Game {
    pub id: u64,
    pub with: Pub,
    pub kind: Kind,
    pub moves: Vec<u8>,
    /// I invited, so I move first (pieces "1").
    pub i_start: bool,
    /// Last time we sent our state (re-sent while waiting, in case a message got lost).
    pub sent: Instant,
}

impl Game {
    pub fn me(&self) -> u8 {
        if self.i_start { 1 } else { 2 }
    }
    pub fn board(&self) -> Vec<u8> {
        board(self.kind, &self.moves).unwrap_or_else(|| vec![0; self.kind.size().0 * self.kind.size().1])
    }
    pub fn outcome(&self) -> Outcome {
        outcome(self.kind, &self.board())
    }
    pub fn my_turn(&self) -> bool {
        self.outcome() == Outcome::Playing && (self.moves.len() % 2 == 0) == self.i_start
    }
    /// Plays a move if it's legal and my turn.
    pub fn play(&mut self, m: u8) -> bool {
        let mut next = self.moves.clone();
        next.push(m);
        if !self.my_turn() || board(self.kind, &next).is_none() {
            return false;
        }
        self.moves = next;
        true
    }
    /// The friend's copy arrived: take it if it's ours plus legal moves.
    pub fn update(&mut self, moves: Vec<u8>) -> bool {
        if moves.len() > self.moves.len() && moves.starts_with(&self.moves) && board(self.kind, &moves).is_some() {
            self.moves = moves;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tic_tac_toe() {
        // X: 0,1,2 across the top; O: 3,4.
        let b = board(Kind::TicTacToe, &[0, 3, 1, 4, 2]).unwrap();
        assert_eq!(outcome(Kind::TicTacToe, &b), Outcome::Won(1));
        assert!(board(Kind::TicTacToe, &[0, 0]).is_none(), "taken cell");
        assert!(board(Kind::TicTacToe, &[0, 3, 1, 4, 2, 5]).is_none(), "move after a win");
        let draw = board(Kind::TicTacToe, &[0, 1, 2, 4, 3, 5, 7, 6, 8]).unwrap();
        assert_eq!(outcome(Kind::TicTacToe, &draw), Outcome::Draw);
    }

    #[test]
    fn connect_four() {
        // Player 1 stacks column 0; player 2 plays column 1.
        let b = board(Kind::ConnectFour, &[0, 1, 0, 1, 0, 1]).unwrap();
        assert_eq!(outcome(Kind::ConnectFour, &b), Outcome::Playing);
        assert_eq!(b[5 * 7], 1, "pieces drop to the bottom row");
        let b = board(Kind::ConnectFour, &[0, 1, 0, 1, 0, 1, 0]).unwrap();
        assert_eq!(outcome(Kind::ConnectFour, &b), Outcome::Won(1));
        // Diagonal for player 2: bottom-left to top-right.
        let b = board(Kind::ConnectFour, &[1, 0, 2, 1, 3, 2, 3, 6, 3, 2, 6, 3]).unwrap();
        assert_eq!(outcome(Kind::ConnectFour, &b), Outcome::Won(2));
        assert!(board(Kind::ConnectFour, &[7]).is_none(), "no such column");
        assert!(board(Kind::ConnectFour, &[0, 0, 0, 0, 0, 0, 0]).is_none(), "full column");
    }

    #[test]
    fn turns_and_sync() {
        let p = [0u8; 32];
        let mut host = Game { id: 1, with: p, kind: Kind::TicTacToe, moves: vec![], i_start: true, sent: Instant::now() };
        let mut guest = Game { id: 1, with: p, kind: Kind::TicTacToe, moves: vec![], i_start: false, sent: Instant::now() };
        assert!(!guest.play(4), "not the guest's turn");
        assert!(host.play(4));
        assert!(!host.play(0), "can't move twice");
        assert!(guest.update(host.moves.clone()));
        assert!(!guest.update(host.moves.clone()), "a repeat changes nothing");
        assert!(guest.play(0));
        assert!(!host.update(vec![1, 0]), "a different history is ignored");
        assert!(host.update(guest.moves.clone()));
        assert!(host.my_turn());
    }
}

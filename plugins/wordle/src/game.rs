//! 游戏状态与标准 Wordle 判定。

use std::collections::HashSet;

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;

pub const WORD_LEN: usize = 5;
pub const MAX_GUESSES: usize = 6;

/// 单格反馈：绿（位置正确）/ 黄（字母在答案中但位置不对）/ 灰（不在答案中）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tile {
    Correct,
    Present,
    Absent,
}

/// 标准 Wordle 判定，正确处理重复字母：
/// 先标记位置正确的格子并扣除对应字母，剩余字母再按出现次数分配黄色。
pub fn evaluate(guess: &str, answer: &str) -> [Tile; WORD_LEN] {
    debug_assert!(guess.len() == WORD_LEN && answer.len() == WORD_LEN);
    let guess = guess.as_bytes();
    let answer = answer.as_bytes();

    let mut result = [Tile::Absent; WORD_LEN];
    let mut remaining = [0u8; 26];
    for (i, &b) in answer.iter().enumerate() {
        if guess[i] == b {
            result[i] = Tile::Correct;
        } else {
            remaining[(b - b'a') as usize] += 1;
        }
    }
    for (i, &g) in guess.iter().enumerate() {
        if result[i] == Tile::Correct {
            continue;
        }
        let idx = (g - b'a') as usize;
        if remaining[idx] > 0 {
            result[i] = Tile::Present;
            remaining[idx] -= 1;
        }
    }
    result
}

/// 从答案池确定性选词：同一 seed 始终得到同一答案，便于测试与复现。
pub fn pick_answer(answers: &[String], seed: u64) -> &str {
    let mut rng = StdRng::seed_from_u64(seed);
    answers.choose(&mut rng).expect("答案池不能为空").as_str()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    InvalidLength,
    NotInWordList,
    GameOver,
}

/// 一局游戏：记录答案、每次猜测的反馈与胜负。
///
/// `guesses` 保留历史猜词，供渲染端（CLI 终端回显 / 插件图片网格）使用。
#[derive(Debug, Clone)]
pub struct Game {
    answer: String,
    guesses: Vec<String>,
    tiles: Vec<[Tile; WORD_LEN]>,
    won: bool,
}

impl Game {
    pub fn new(answer: String) -> Self {
        Self {
            answer,
            guesses: Vec::new(),
            tiles: Vec::new(),
            won: false,
        }
    }

    /// 提交一次猜测。`allowed` 为允许猜测的全集（含答案词）。
    ///
    /// 合法时记录反馈并返回；非法输入不消耗次数。
    pub fn submit(
        &mut self,
        guess: &str,
        allowed: &HashSet<String>,
    ) -> Result<[Tile; WORD_LEN], SubmitError> {
        if self.is_over() {
            return Err(SubmitError::GameOver);
        }
        if guess.len() != WORD_LEN {
            return Err(SubmitError::InvalidLength);
        }
        if !allowed.contains(guess) {
            return Err(SubmitError::NotInWordList);
        }
        let tiles = evaluate(guess, &self.answer);
        self.won |= tiles.iter().all(|&t| t == Tile::Correct);
        self.guesses.push(guess.to_owned());
        self.tiles.push(tiles);
        Ok(tiles)
    }

    pub fn is_won(&self) -> bool {
        self.won
    }

    pub fn is_over(&self) -> bool {
        self.won || self.tiles.len() >= MAX_GUESSES
    }

    pub fn remaining(&self) -> usize {
        MAX_GUESSES - self.tiles.len()
    }

    pub fn guesses_count(&self) -> usize {
        self.tiles.len()
    }

    pub fn answer(&self) -> &str {
        &self.answer
    }

    pub fn tiles(&self) -> &[[Tile; WORD_LEN]] {
        &self.tiles
    }

    /// 每次合法猜测的原文，与 [`Self::tiles`] 一一对应。
    pub fn guesses(&self) -> &[String] {
        &self.guesses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiles_str(tiles: &[Tile]) -> String {
        tiles
            .iter()
            .map(|t| match t {
                Tile::Correct => 'G',
                Tile::Present => 'Y',
                Tile::Absent => '.',
            })
            .collect()
    }

    #[test]
    fn evaluate_basic() {
        // 标准示例：slate vs crane → a/e 位置正确，其余缺席
        assert_eq!(tiles_str(&evaluate("slate", "crane")), "..G.G");
        // 全中
        assert_eq!(tiles_str(&evaluate("crane", "crane")), "GGGGG");
        // 全缺席
        assert_eq!(tiles_str(&evaluate("xyzzy", "crane")), ".....");
    }

    #[test]
    fn evaluate_present_letters() {
        // serve vs crane → e 只有一个且在位置 4 命中（绿），位置 1 的 e 因计数耗尽为灰；
        // r 在答案中但位置不对 → 黄
        assert_eq!(tiles_str(&evaluate("serve", "crane")), "..Y.G");
    }

    #[test]
    fn evaluate_duplicate_letters() {
        // 猜词重复：答案 abcde，猜 aaccc
        // 位置0 a 绿；位置1 a 灰（答案只有一个 a 且已被位置0 占用）；位置2 c 绿；
        // 位置3/4 c 灰（答案只有一个 c 且已被位置2 占用）
        assert_eq!(tiles_str(&evaluate("aaccc", "abcde")), "G.G..");
        // 答案重复：答案 aaabb，猜 abbbb
        // 位置0 a 绿；位置1/2 b 灰（答案只有两个 b，均在位置3/4 命中）；位置3/4 b 绿
        assert_eq!(tiles_str(&evaluate("abbbb", "aaabb")), "G..GG");
    }

    #[test]
    fn pick_answer_is_deterministic() {
        let answers: Vec<String> = (0..1000).map(|i| format!("a{:04}", i)).collect();
        let a = pick_answer(&answers, 42);
        let b = pick_answer(&answers, 42);
        assert_eq!(a, b);
        // 不同 seed 大概率不同（词表足够大时）
        let c = pick_answer(&answers, 43);
        assert_ne!(a, c);
    }

    #[test]
    fn game_flow() {
        let mut allowed: HashSet<String> = ["crane", "slate", "xyzzy"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let answer = "crane".to_owned();
        allowed.insert(answer.clone());
        let mut game = Game::new(answer);

        assert_eq!(
            game.submit("slate", &allowed),
            Ok([
                Tile::Absent,
                Tile::Absent,
                Tile::Correct,
                Tile::Absent,
                Tile::Correct
            ])
        );
        assert!(!game.is_won() && !game.is_over());
        assert_eq!(game.remaining(), 5);

        // 非法输入不消耗次数
        assert_eq!(
            game.submit("abcd", &allowed),
            Err(SubmitError::InvalidLength)
        );
        assert_eq!(
            game.submit("qqqqq", &allowed),
            Err(SubmitError::NotInWordList)
        );
        assert_eq!(game.remaining(), 5);

        assert_eq!(
            game.submit("crane", &allowed),
            Ok([Tile::Correct; WORD_LEN])
        );
        assert!(game.is_won() && game.is_over());
        assert_eq!(game.submit("slate", &allowed), Err(SubmitError::GameOver));
    }

    #[test]
    fn game_exhausts_after_six_guesses() {
        let mut allowed: HashSet<String> = ["crane", "slate", "xyzzy"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let answer = "crane".to_owned();
        allowed.insert(answer.clone());
        let mut game = Game::new(answer);
        for _ in 0..6 {
            assert!(!game.is_over());
            game.submit("xyzzy", &allowed).unwrap();
        }
        assert!(game.is_over() && !game.is_won());
    }
}

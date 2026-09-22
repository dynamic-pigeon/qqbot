//! 游戏状态与标准 Wordle 判定。

use std::collections::{BTreeMap, HashSet};

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;

pub const WORD_LEN: usize = 5;
pub const MAX_GUESSES: usize = 6;

/// 单格反馈：绿（位置正确）/ 黄（字母在答案中但位置不对）/ 灰（不在答案中）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
///
/// 两种模式：
/// - 普通：答案开局固定（`Fixed`）；
/// - 严格（对抗）：答案不预先固定，每次猜测后从与历史反馈一致的候选池中
///   按稀释信息的程度加权选取新答案，较大的反馈桶更容易被抽中。
#[derive(Debug, Clone)]
pub struct Game {
    mode: Mode,
    guesses: Vec<String>,
    tiles: Vec<[Tile; WORD_LEN]>,
    won: bool,
}

#[derive(Debug, Clone)]
enum Mode {
    Fixed {
        answer: String,
    },
    Adversarial {
        candidates: Vec<String>,
        /// 最近一轮反馈对应的名义答案（猜中后即真实答案）。
        current: Option<String>,
    },
}

impl Game {
    pub fn new(answer: String) -> Self {
        Self {
            mode: Mode::Fixed { answer },
            guesses: Vec::new(),
            tiles: Vec::new(),
            won: false,
        }
    }

    /// 严格模式：以整个答案池作为候选，答案随猜测动态确定。
    pub fn new_adversarial(answers: Vec<String>) -> Self {
        Self {
            mode: Mode::Adversarial {
                candidates: answers,
                current: None,
            },
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
        let tiles = match &mut self.mode {
            Mode::Fixed { answer } => evaluate(guess, answer),
            Mode::Adversarial {
                candidates,
                current,
            } => {
                let (tiles, chosen) = adversarial_pick(candidates, guess);
                *current = Some(chosen);
                tiles
            }
        };
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

    /// 当前答案；严格模式未收敛前是最近一轮反馈对应的候选词。
    pub fn answer(&self) -> &str {
        match &self.mode {
            Mode::Fixed { answer } => answer,
            Mode::Adversarial { current, .. } => current.as_deref().unwrap_or_default(),
        }
    }

    pub fn tiles(&self) -> &[[Tile; WORD_LEN]] {
        &self.tiles
    }

    /// 每次合法猜测的原文，与 [`Self::tiles`] 一一对应。
    pub fn guesses(&self) -> &[String] {
        &self.guesses
    }

    pub fn is_adversarial(&self) -> bool {
        matches!(self.mode, Mode::Adversarial { .. })
    }

    /// 严格模式下的候选池（与历史反馈一致的答案）；普通模式为空。
    pub fn candidates(&self) -> &[String] {
        match &self.mode {
            Mode::Fixed { .. } => &[],
            Mode::Adversarial { candidates, .. } => candidates,
        }
    }

    /// 严格模式下与全部历史反馈一致的候选词数量；普通模式恒为 1。
    pub fn candidates_remaining(&self) -> usize {
        match &self.mode {
            Mode::Fixed { .. } => 1,
            Mode::Adversarial { candidates, .. } => candidates.len(),
        }
    }

    /// 游戏结束后的统一展示文本（胜/负），CLI 与 QQ 插件共用。
    ///
    /// `meaning` 为答案释义，只在展示确定答案的分支追加；
    /// 严格模式用尽仍未收敛时答案不确定，如实告知剩余候选数，不附释义。
    pub fn result_note(&self, meaning: Option<&str>) -> Option<String> {
        if !self.is_over() {
            return None;
        }
        let answer = self.answer().to_ascii_uppercase();
        let gloss = meaning.map(|m| format!("\n📖 {m}")).unwrap_or_default();
        if self.is_won() {
            Some(format!(
                "🎉 恭喜猜中！答案是 {answer}，共用 {} 次{gloss}",
                self.guesses_count()
            ))
        } else if self.is_adversarial() && self.candidates_remaining() > 1 {
            Some(format!(
                "😞 次数用尽！严格模式下答案未收敛（剩余 {} 个候选）",
                self.candidates_remaining()
            ))
        } else {
            Some(format!("😞 次数用尽，答案是 {answer}{gloss}"))
        }
    }
}

/// 严格模式：按反馈分桶，候选多于 1 时排除全绿，再按桶大小加权抽取。
/// 越大的桶越能稀释信息；权重 `4096 >> 排名`，同大小同权重，每落后一档减半，12 档以外为 1。
/// 候选池只剩 1 个词时只能选它。
fn adversarial_pick(candidates: &mut Vec<String>, guess: &str) -> ([Tile; WORD_LEN], String) {
    use rand::rng;

    let all_green = [Tile::Correct; WORD_LEN];
    let mut buckets: BTreeMap<[Tile; WORD_LEN], Vec<String>> = BTreeMap::new();
    for cand in candidates.iter() {
        buckets
            .entry(evaluate(guess, cand))
            .or_default()
            .push(cand.clone());
    }

    let entries: Vec<([Tile; WORD_LEN], Vec<String>)> = buckets
        .into_iter()
        .filter(|(fb, _)| !(candidates.len() > 1 && *fb == all_green))
        .collect();
    let mut sizes: Vec<usize> = entries.iter().map(|(_, words)| words.len()).collect();
    sizes.sort_unstable();
    sizes.dedup();
    let (tiles, chosen) = entries
        .choose_weighted(&mut rng(), |(_, words)| {
            4096u32 >> (sizes.iter().filter(|&&s| s > words.len()).count() as u32).min(12)
        })
        .ok()
        .cloned()
        .unwrap_or_else(|| (all_green, vec![guess.to_owned()]));

    *candidates = chosen;
    let current = candidates
        .choose(&mut rng())
        .cloned()
        .unwrap_or_else(|| guess.to_owned());
    (tiles, current)
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

    fn adversarial_allowed() -> HashSet<String> {
        ["crane", "slate", "serve", "bible", "story", "abcde"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn adversarial_pool_never_converges_to_green_while_many_candidates() {
        let allowed = adversarial_allowed();
        let answers: Vec<String> = allowed.iter().cloned().collect();
        let game = Game::new_adversarial(answers);
        // 候选池 > 1 时，任何猜测都不应直接全绿（对抗策略排除全绿桶）
        for guess in ["crane", "slate", "serve", "bible"] {
            let mut g = game.clone();
            let tiles = g.submit(guess, &allowed).unwrap();
            assert_ne!(tiles, [Tile::Correct; WORD_LEN], "{guess} 不应直接猜中");
            assert!(!g.is_won());
            assert!(g.candidates_remaining() >= 1);
        }
    }

    #[test]
    fn adversarial_feedback_is_consistent_with_some_candidate() {
        let allowed = adversarial_allowed();
        let answers: Vec<String> = allowed.iter().cloned().collect();
        let mut game = Game::new_adversarial(answers);
        // 每轮反馈必须与候选池中至少一个词一致（自洽性）
        for guess in ["crane", "slate", "serve", "bible", "story", "abcde"] {
            if game.is_over() {
                break; // 对抗局可能提前收敛结束
            }
            let tiles = game.submit(guess, &allowed).unwrap();
            let consistent = game
                .candidates()
                .iter()
                .any(|cand| evaluate(guess, cand) == tiles);
            assert!(consistent, "猜测 {guess} 的反馈与候选池不一致");
            assert!(!game.is_won() || game.candidates_remaining() == 1);
        }
    }

    #[test]
    fn adversarial_wins_when_single_candidate_left() {
        let allowed = adversarial_allowed();
        let mut game = Game::new_adversarial(vec!["crane".to_owned()]);
        let tiles = game.submit("crane", &allowed).unwrap();
        assert_eq!(tiles, [Tile::Correct; WORD_LEN]);
        assert!(game.is_won());
        assert_eq!(game.answer(), "crane");
        assert_eq!(
            game.result_note(None).unwrap(),
            "🎉 恭喜猜中！答案是 CRANE，共用 1 次"
        );
    }

    #[test]
    fn result_note_appends_meaning_when_answer_settled() {
        let mut allowed: HashSet<String> =
            ["crane", "xyzzy"].into_iter().map(str::to_owned).collect();
        allowed.insert("crane".to_owned());
        let mut game = Game::new("crane".to_owned());
        for _ in 0..6 {
            game.submit("xyzzy", &allowed).unwrap();
        }
        assert_eq!(
            game.result_note(Some("n. 鹤, 起重机")).unwrap(),
            "😞 次数用尽，答案是 CRANE\n📖 n. 鹤, 起重机"
        );
        assert_eq!(
            game.result_note(None).unwrap(),
            "😞 次数用尽，答案是 CRANE",
            "无释义时保持原文"
        );
    }

    #[test]
    fn adversarial_exhaust_reports_remaining_candidates() {
        let allowed = adversarial_allowed();
        let answers: Vec<String> = allowed.iter().cloned().collect();
        let mut game = Game::new_adversarial(answers);
        // 用"差"的猜词尽量不收敛：固定猜与所有候选都不同反馈的词
        for _ in 0..6 {
            let _ = game.submit("abcde", &allowed).unwrap();
        }
        assert!(game.is_over());
        let note = game.result_note(None).unwrap();
        if !game.is_won() {
            assert!(note.contains("未收敛") || note.contains("答案是"), "{note}");
        }
    }

    #[test]
    fn adversarial_result_note_requires_game_over() {
        let allowed = adversarial_allowed();
        let mut game = Game::new_adversarial(allowed.iter().cloned().collect());
        assert_eq!(game.result_note(None), None);
        game.submit("crane", &allowed).unwrap();
        assert_eq!(game.result_note(None), None, "未结束时无结果文本");
    }
}

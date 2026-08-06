//! Wordle CLI：交互式猜词。

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use wordle::game::{Game, MAX_GUESSES, SubmitError, Tile, WORD_LEN, pick_answer};
use wordle::words::load_or_download;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let args = parse_args()?;
    let word_list = load_or_download(&args.data_dir)
        .await
        .context("词库加载失败")?;

    let mut game = if args.hard {
        println!("🔤 英文 Wordle（严格模式）— 答案不固定，随你的猜测动态变化");
        Game::new_adversarial(word_list.answers.clone())
    } else {
        let answer = match &args.answer {
            Some(word) => {
                if !word_list.allowed.contains(word) {
                    bail!("指定的答案不在词库中");
                }
                word.clone()
            }
            None => pick_answer(&word_list.answers, args.seed).to_owned(),
        };
        Game::new(answer)
    };

    println!("🔤 英文 Wordle — 猜一个 {WORD_LEN} 字母单词，共 {MAX_GUESSES} 次机会");
    println!("🟩 位置正确  🟨 字母在答案中  ⬛ 不在答案中");
    if !args.hard && args.answer.is_none() {
        println!("（调试参数：--answer <词> 指定答案，--seed <数字> 固定随机答案）");
    }
    println!();

    while !game.is_over() {
        let Some(guess) = prompt(&format!(
            "第 {}/{} 次猜测: ",
            game.guesses_count() + 1,
            MAX_GUESSES
        ))?
        else {
            break;
        };
        match game.submit(&guess, &word_list.allowed) {
            Err(SubmitError::InvalidLength) => {
                println!("⚠️  请输入恰好 {WORD_LEN} 个字母");
            }
            Err(SubmitError::NotInWordList) => {
                println!("⚠️  “{guess}”不在词表中");
            }
            Err(SubmitError::GameOver) => break,
            Ok(tiles) => {
                print_tiles(&guess, &tiles);
                println!();
            }
        }
    }

    if let Some(note) = game.result_note() {
        println!("\n{note}");
    }
    Ok(())
}

struct Args {
    seed: u64,
    answer: Option<String>,
    hard: bool,
    data_dir: PathBuf,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut seed = 0u64;
    let mut answer = None;
    let mut hard = false;
    let mut data_dir = PathBuf::from("data");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => {
                let value = args.next().with_context(|| "--seed 需要一个数字参数")?;
                seed = value
                    .parse()
                    .with_context(|| format!("--seed 参数 {value:?} 不是数字"))?;
            }
            "--answer" => {
                let value = args.next().with_context(|| "--answer 需要一个单词参数")?;
                answer = Some(value.to_ascii_lowercase());
            }
            "--hard" => hard = true,
            "--data-dir" => {
                let value = args.next().with_context(|| "--data-dir 需要一个路径参数")?;
                data_dir = PathBuf::from(value);
            }
            "--help" | "-h" => {
                println!(
                    "用法: wordle [--seed <数字>] [--answer <单词>] [--hard] [--data-dir <路径>]"
                );
                std::process::exit(0);
            }
            other => bail!("未知参数: {other}（--help 查看用法）"),
        }
    }
    Ok(Args {
        seed,
        answer,
        hard,
        data_dir,
    })
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}

/// 读一行输入：trim 并转小写；EOF 返回 `None`，空行继续等待。
fn prompt(label: &str) -> anyhow::Result<Option<String>> {
    loop {
        print!("{label}");
        io::stdout().flush().context("刷新输出失败")?;
        let mut line = String::new();
        let n = io::stdin().read_line(&mut line).context("读取输入失败")?;
        if n == 0 {
            // EOF（Ctrl+D / 管道结束）：由调用方决定如何收尾
            return Ok(None);
        }
        let guess = line.trim().to_ascii_lowercase();
        if !guess.is_empty() {
            return Ok(Some(guess));
        }
    }
}

fn print_tiles(guess: &str, tiles: &[Tile]) {
    let blocks: String = tiles
        .iter()
        .map(|t| match t {
            Tile::Correct => '🟩',
            Tile::Present => '🟨',
            Tile::Absent => '⬛',
        })
        .collect();
    println!("{blocks}  {}", guess.to_ascii_uppercase());
}

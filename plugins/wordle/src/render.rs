//! 纯 Rust 网格渲染：用 image 直接绘制 PNG（色块 + 字母），不依赖浏览器。
//!
//! 布局与配色对齐原版 Wordle 视觉：56px 色块、8px 间距、16px 内边距，
//! 绿 #6aaa64 / 黄 #c9b458 / 灰 #787c7e，空行白底灰边框 #d3d6da。

use std::sync::LazyLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::{ImageFormat, Pixel, Rgba, RgbaImage};

use crate::game::{Game, MAX_GUESSES, Tile, WORD_LEN};

const TILE_PX: u32 = 56;
const GAP_PX: u32 = 8;
const PAD_PX: u32 = 16;
const BORDER_PX: u32 = 2;

const BOARD_W: u32 = PAD_PX * 2 + WORD_LEN as u32 * TILE_PX + (WORD_LEN as u32 - 1) * GAP_PX;
const BOARD_H: u32 = PAD_PX * 2 + MAX_GUESSES as u32 * TILE_PX + (MAX_GUESSES as u32 - 1) * GAP_PX;

const GREEN: Rgba<u8> = Rgba([0x6a, 0xaa, 0x64, 0xff]);
const YELLOW: Rgba<u8> = Rgba([0xc9, 0xb4, 0x58, 0xff]);
const GRAY: Rgba<u8> = Rgba([0x78, 0x7c, 0x7e, 0xff]);
const BORDER: Rgba<u8> = Rgba([0xd3, 0xd6, 0xda, 0xff]);
const WHITE: Rgba<u8> = Rgba([0xff, 0xff, 0xff, 0xff]);

/// 字母字号：OpenSans Bold 的视觉高度略小于方块，避免撑满。
const FONT_PX: f32 = 34.0;

/// 内嵌字体：OpenSans-Bold（SIL OFL 1.1，见 assets/OFL.txt）。
/// 与词云插件同源同授权，避免运行时依赖系统字体。
static FONT_BYTES: &[u8] = include_bytes!("../assets/OpenSans-Bold.ttf");

/// 字体解析一次并缓存：147KB TTF 的 parse 不便宜，每局要渲染多次。
static FONT: LazyLock<FontRef<'static>> =
    LazyLock::new(|| FontRef::try_from_slice(FONT_BYTES).expect("内嵌字体损坏"));

/// 把一局游戏渲染为 PNG 字节：已猜行 = 色块 + 大写字母，未猜行 = 空边框格。
pub fn render_board_png(game: &Game) -> Vec<u8> {
    let mut img = RgbaImage::from_pixel(BOARD_W, BOARD_H, WHITE);
    let font = &*FONT;
    let scale = PxScale::from(FONT_PX);

    for row in 0..MAX_GUESSES {
        let tiles = game.tiles().get(row);
        let guess = game.guesses().get(row).map(String::as_str);
        for col in 0..WORD_LEN {
            let x = PAD_PX + col as u32 * (TILE_PX + GAP_PX);
            let y = PAD_PX + row as u32 * (TILE_PX + GAP_PX);
            let tile = tiles.map(|tiles| tiles[col]);
            draw_tile(
                &mut img,
                x,
                y,
                tile,
                guess.and_then(|g| g.chars().nth(col)),
                font,
                scale,
            );
        }
    }

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .expect("PNG 编码失败");
    png
}

fn draw_tile(
    img: &mut RgbaImage,
    x: u32,
    y: u32,
    tile: Option<Tile>,
    letter: Option<char>,
    font: &FontRef<'_>,
    scale: PxScale,
) {
    // 背景色块铺满整格；空行保持白底并画灰边框。
    let bg = match tile {
        Some(Tile::Correct) => GREEN,
        Some(Tile::Present) => YELLOW,
        Some(Tile::Absent) => GRAY,
        None => WHITE,
    };
    for py in y..y + TILE_PX {
        for px in x..x + TILE_PX {
            img.put_pixel(px, py, bg);
        }
    }
    if tile.is_none() {
        for b in 0..BORDER_PX {
            draw_hollow_rect(img, x + b, y + b, TILE_PX - b * 2, TILE_PX - b * 2, BORDER);
        }
    }

    if let Some(letter) = letter {
        // 水平按 h_advance、垂直按 em 高居中，和字号定义一致。
        let (tw, th) = glyph_size(font, scale, letter);
        let lx = x as i32 + (TILE_PX as i32 - tw as i32) / 2;
        let ly = y as i32 + (TILE_PX as i32 - th as i32) / 2;
        draw_glyph(img, WHITE, lx, ly, scale, font, letter);
    }
}

fn draw_hollow_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if w == 0 || h == 0 {
        return;
    }
    let x1 = x + w - 1;
    let y1 = y + h - 1;
    for px in x..=x1 {
        img.put_pixel(px, y, color);
        img.put_pixel(px, y1, color);
    }
    for py in y..=y1 {
        img.put_pixel(x, py, color);
        img.put_pixel(x1, py, color);
    }
}

fn glyph_size(font: &FontRef<'_>, scale: PxScale, ch: char) -> (u32, u32) {
    let scaled = font.as_scaled(scale);
    let w = scaled.h_advance(scaled.glyph_id(ch)).ceil().max(0.0) as u32 + 1;
    let h = scaled.height().ceil().max(0.0) as u32;
    (w, h)
}

fn draw_glyph(
    img: &mut RgbaImage,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    scale: PxScale,
    font: &FontRef<'_>,
    ch: char,
) {
    let scaled = font.as_scaled(scale);
    let glyph_id = scaled.glyph_id(ch);
    let glyph = glyph_id.with_scale_and_position(scale, point(0.0, scaled.ascent()));
    let Some(outlined) = font.outline_glyph(glyph) else {
        return;
    };
    let bounds = outlined.px_bounds();
    let x_shift = x + bounds.min.x.round() as i32;
    let y_shift = y + bounds.min.y.round() as i32;
    let width = img.width() as i32;
    let height = img.height() as i32;
    outlined.draw(|gx, gy, coverage| {
        let px = gx as i32 + x_shift;
        let py = gy as i32 + y_shift;
        if !(0..width).contains(&px) || !(0..height).contains(&py) {
            return;
        }
        let coverage = coverage.clamp(0.0, 1.0);
        let px = px as u32;
        let py = py as u32;
        let mut pixel = *img.get_pixel(px, py);
        let overlay = Rgba([
            color[0],
            color[1],
            color[2],
            (color[3] as f32 * coverage) as u8,
        ]);
        pixel.blend(&overlay);
        img.put_pixel(px, py, pixel);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn started_game() -> Game {
        let mut allowed: HashSet<String> = ["crane", "slate", "serve"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        allowed.insert("crane".to_owned());
        let mut game = Game::new("crane".to_owned());
        game.submit("slate", &allowed).unwrap();
        game.submit("serve", &allowed).unwrap();
        game
    }

    /// 格子内角点（避开中央字母）的颜色。
    fn corner_color(img: &RgbaImage, row: u32, col: u32) -> Rgba<u8> {
        let x = PAD_PX + col * (TILE_PX + GAP_PX) + 6;
        let y = PAD_PX + row * (TILE_PX + GAP_PX) + 6;
        *img.get_pixel(x, y)
    }

    /// 格子中心像素：可能落在字母笔画上，仅用于尺寸/绘制完整性检查。
    fn center_color(img: &RgbaImage, row: u32, col: u32) -> Rgba<u8> {
        let x = PAD_PX + col * (TILE_PX + GAP_PX) + TILE_PX / 2;
        let y = PAD_PX + row * (TILE_PX + GAP_PX) + TILE_PX / 2;
        *img.get_pixel(x, y)
    }

    fn count_pixels(img: &RgbaImage, color: Rgba<u8>) -> u32 {
        img.pixels().filter(|&&p| p == color).count() as u32
    }

    #[test]
    fn png_has_wordle_board_size() {
        let png = render_board_png(&Game::new("crane".to_owned()));
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (BOARD_W, BOARD_H));
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn guessed_rows_use_tile_colors() {
        let img = image::load_from_memory(&render_board_png(&started_game()))
            .unwrap()
            .to_rgba8();
        // slate vs crane：s 灰、a 绿；serve vs crane：r 黄、e 绿
        assert_eq!(corner_color(&img, 0, 0), GRAY);
        assert_eq!(corner_color(&img, 0, 2), GREEN);
        assert_eq!(corner_color(&img, 1, 2), YELLOW);
        assert_eq!(corner_color(&img, 1, 4), GREEN);
    }

    #[test]
    fn empty_rows_are_white_with_border() {
        let img = image::load_from_memory(&render_board_png(&Game::new("crane".to_owned())))
            .unwrap()
            .to_rgba8();
        assert_eq!(corner_color(&img, 0, 0), WHITE);
        assert_eq!(corner_color(&img, 5, 4), WHITE);
        // 边框像素应存在（格子左上角为边框色）
        assert_eq!(
            *img.get_pixel(PAD_PX, PAD_PX),
            BORDER,
            "空行左上角应为边框色"
        );
        // 无任何色块
        assert_eq!(count_pixels(&img, GREEN), 0);
        assert_eq!(count_pixels(&img, YELLOW), 0);
        assert_eq!(count_pixels(&img, GRAY), 0);
    }

    #[test]
    fn letters_are_drawn_in_white() {
        let img = image::load_from_memory(&render_board_png(&started_game()))
            .unwrap()
            .to_rgba8();
        // 已猜行的白色像素只可能来自字母（底是色块）。
        assert!(count_pixels(&img, WHITE) > 0, "应绘制白色字母");
        let empty = image::load_from_memory(&render_board_png(&Game::new("crane".to_owned())))
            .unwrap()
            .to_rgba8();
        // 空盘格子中心是白底，没有字母笔画。
        assert_eq!(center_color(&empty, 0, 0), WHITE);
    }
}

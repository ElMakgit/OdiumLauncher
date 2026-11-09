//! Pixel-art assistant "????" drawn from an embedded 40x44 sprite.
//!
//! Each frame is a grid of palette characters. The sprite is rendered with
//! integer-scaled rectangles, so it stays crisp at every DPI and needs no
//! image decoder or external assets. Two tail frames, a two-pixel idle bob and
//! a periodic blink make the assistant feel alive.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Vec2};

const SPRITE_WIDTH: usize = 40;
const SPRITE_HEIGHT: usize = 44;
const SCALE: f32 = 3.0;

const DOG_MID: [&str; 44] = [
    "........................................",
    "........................................",
    "........................................",
    "....................o...................",
    "...............ooooofooooo..............",
    ".............oolfllllllflloo............",
    ".....ooo....ollfllllllflllllo....ooo....",
    "....oeeeo.oollfllllllfllllllfoo.oeeeo...",
    "...oeeeeeofllfllllllfllllllflffoeeeeeo..",
    "..oeeeeeeefffffffffffffffffffffeeeeeeeo.",
    "..oeeeeeefffffffffffffffffffffffeeeeeeo.",
    ".oeeeeeefffffffffffffffffffffffffeeeeeeo",
    ".oeeeeeefffffffffffffffffffffffffeeeeeeo",
    ".oeeeiiifffffffffffffffffffffffffiiieeeo",
    ".oeeeiiiffffwwwddfffffffwwwddffffiiieeeo",
    "oeeeiiidddffwwwkdfffcfffwwwkdffdddiiieee",
    ".oeeiiiiddffwwkkdcccccccwwkkdffddiiiieeo",
    ".oeeiiiiddffwwkkdcccccccwwkkdffddiiiieeo",
    ".oeeiiiiddffffcccccccccccccffffddiiiieeo",
    ".oeeiiiiddfffccccckwkkccccccfffddiiiieeo",
    "..oeeiiiuufffccccckkkkccccccfffuuiiieeo.",
    "..oeeiiiuufffccccckkkkccccccfffuuiiieeo.",
    "...oeeeeeoffffccccckckcccccffffoeeeeeo..",
    "....oeeeo.oofffccccckcccccfffoo.oeeeo...",
    ".....ooo....offfllrpprlllfffo....ooo....",
    "...........oooofffrpprffffoooo..........",
    "..........oblbblbbrrrrbblbblbbo.........",
    "..........obbbbbbbrrrrbbbbbbbbo.........",
    "..........obbbbbbbbtttbbbbbbbbo.........",
    "...........offfffftwtttffffffo..........",
    "..........offffffftttttfffffffo.........",
    ".........offfffffftttttffffffffo..oooooo",
    ".........offffffffftttfffffffffooofffflf",
    ".........offffffffffffffffffffffffffflll",
    ".........offffffffffffffffffffffffffllll",
    ".........offffffffffffffffffffffffffflll",
    ".........offfffofffffffffofffffffffffflf",
    ".........offfffofffffffffoffffffffffffoo",
    "..........offffofffffffffoffffffffoooo..",
    "..........occccodddddddddoccccoooo......",
    "..........occdcodddddddddocdcco.........",
    "..........occccodddddddddocccco.........",
    "...........ooooooofffffooooooo..........",
    "..................ooooo.................",
];

const DOG_DOWN: [&str; 44] = [
    "........................................",
    "........................................",
    "........................................",
    "....................o...................",
    "...............ooooofooooo..............",
    ".............oolfllllllflloo............",
    ".....ooo....ollfllllllflllllo....ooo....",
    "....oeeeo.oollfllllllfllllllfoo.oeeeo...",
    "...oeeeeeofllfllllllfllllllflffoeeeeeo..",
    "..oeeeeeeefffffffffffffffffffffeeeeeeeo.",
    "..oeeeeeefffffffffffffffffffffffeeeeeeo.",
    ".oeeeeeefffffffffffffffffffffffffeeeeeeo",
    ".oeeeeeefffffffffffffffffffffffffeeeeeeo",
    ".oeeeiiifffffffffffffffffffffffffiiieeeo",
    ".oeeeiiiffffwwwddfffffffwwwddffffiiieeeo",
    "oeeeiiidddffwwwkdfffcfffwwwkdffdddiiieee",
    ".oeeiiiiddffwwkkdcccccccwwkkdffddiiiieeo",
    ".oeeiiiiddffwwkkdcccccccwwkkdffddiiiieeo",
    ".oeeiiiiddffffcccccccccccccffffddiiiieeo",
    ".oeeiiiiddfffccccckwkkccccccfffddiiiieeo",
    "..oeeiiiuufffccccckkkkccccccfffuuiiieeo.",
    "..oeeiiiuufffccccckkkkccccccfffuuiiieeo.",
    "...oeeeeeoffffccccckckcccccffffoeeeeeo..",
    "....oeeeo.oofffccccckcccccfffoo.oeeeo...",
    ".....ooo....offfllrpprlllfffo....ooo....",
    "...........oooofffrpprffffoooo..........",
    "..........oblbblbbrrrrbblbblbbo.........",
    "..........obbbbbbbrrrrbbbbbbbbo.........",
    "..........obbbbbbbbtttbbbbbbbbo.........",
    "...........offfffftwtttffffffo..........",
    "..........offffffftttttfffffffo.........",
    ".........offfffffftttttffffffffo........",
    ".........offffffffftttfffffffffo........",
    ".........offfffffffffffffffffffoo.......",
    ".........offfffffffffffffffffffffoooo...",
    ".........offfffffffffffffffffffffffffoo.",
    ".........offfffofffffffffofffffffffflffo",
    ".........offfffofffffffffoffffffffflllfo",
    "..........offffofffffffffofffffffflllllo",
    "..........occccodddddddddoccccdfffflllfo",
    "..........occdcodddddddddocdccoooffflffo",
    "..........occccodddddddddocccco..oooooo.",
    "...........ooooooofffffooooooo..........",
    "..................ooooo.................",
];

const OUTLINE: Color32 = Color32::from_rgb(0x2B, 0x1B, 0x0E);
const FUR: Color32 = Color32::from_rgb(0xE8, 0xB8, 0x4B);

fn palette(pixel: char) -> Option<Color32> {
    Some(match pixel {
        '.' => return None,
        'o' => OUTLINE,
        'f' => FUR,
        'd' => Color32::from_rgb(0xC9, 0x94, 0x2F),
        'l' => Color32::from_rgb(0xF7, 0xDF, 0x9B),
        'c' => Color32::from_rgb(0xFF, 0xEE, 0xC4),
        'e' => Color32::from_rgb(0xB8, 0x7E, 0x26),
        'i' => Color32::from_rgb(0xE8, 0x8A, 0x9A),
        'w' => Color32::WHITE,
        'k' => Color32::from_rgb(0x1A, 0x12, 0x08),
        'p' => Color32::from_rgb(0xE6, 0x78, 0x8C),
        'r' => Color32::from_rgb(0xD9, 0x53, 0x6A),
        'b' => Color32::from_rgb(0x24, 0x3B, 0x8F),
        't' => Color32::from_rgb(0xFF, 0xD2, 0x4A),
        'u' => Color32::from_rgb(0xF4, 0x9A, 0x9A),
        _ => return None,
    })
}

/// Draws the assistant. `time` is egui's wall-clock seconds; tail, bob and
/// blink all derive from it so the caller only has to request repaints.
pub(crate) fn draw_assistant(ui: &mut egui::Ui, time: f64) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(
            SPRITE_WIDTH as f32 * SCALE,
            SPRITE_HEIGHT as f32 * SCALE + 16.0,
        ),
        Sense::hover(),
    );

    // A slow two-frame wag: about one full left/right cycle per second.
    let sprite = if (time * 3.2).sin() > 0.0 {
        &DOG_DOWN
    } else {
        &DOG_MID
    };
    // Snap the bob to whole sprite pixels so the art never blurs.
    let bob = if (time * 2.4).sin() > 0.0 { -1.0 } else { 0.0 };
    let origin = Pos2::new(
        rect.center().x - SPRITE_WIDTH as f32 * SCALE / 2.0,
        rect.top() + 2.0 + bob * SCALE,
    );
    let painter = ui.painter();

    for (y, row) in sprite.iter().enumerate() {
        let pixels: Vec<char> = row.chars().collect();
        let mut x = 0;
        while x < pixels.len() {
            let pixel = pixels[x];
            let start = x;
            while x < pixels.len() && pixels[x] == pixel {
                x += 1;
            }
            if let Some(color) = palette(pixel) {
                let min = Pos2::new(origin.x + start as f32 * SCALE, origin.y + y as f32 * SCALE);
                let size = Vec2::new((x - start) as f32 * SCALE, SCALE);
                painter.rect_filled(Rect::from_min_size(min, size), egui::Rounding::ZERO, color);
            }
        }
    }

    // Blink for ~0.18 s every 3.4 s. Covering the eye rectangles with fur and
    // drawing a dark lid keeps the silhouette intact.
    if (time % 3.4) > 3.22 {
        for eye_left in [12.0_f32, 24.0] {
            let eye = Rect::from_min_size(
                Pos2::new(origin.x + eye_left * SCALE, origin.y + 14.0 * SCALE),
                Vec2::new(5.0 * SCALE, 4.0 * SCALE),
            );
            painter.rect_filled(eye, egui::Rounding::ZERO, FUR);
            let lid = Rect::from_min_size(
                Pos2::new(eye.left(), origin.y + 16.0 * SCALE),
                Vec2::new(5.0 * SCALE, SCALE),
            );
            painter.rect_filled(lid, egui::Rounding::ZERO, OUTLINE);
        }
    }
}

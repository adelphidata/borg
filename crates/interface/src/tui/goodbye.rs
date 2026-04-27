use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use super::theme;

const FAREWELL: &[&str] = &[
    r"oooooooooooo       .o.       ooooooooo.   oooooooooooo oooooo   oooooo     oooo oooooooooooo ooooo        ooooo       ",
    r"`888'     `8      .888.      `888   `Y88. `888'     `8  `888.    `888.     .8'  `888'     `8 `888'        `888'       ",
    r" 888             .8'888.      888   .d88'  888           `888.   .8888.   .8'    888          888          888        ",
    r" 888oooo8       .8' `888.     888ooo88P'   888oooo8       `888  .8'`888. .8'     888oooo8     888          888        ",
    r" 888    '      .88ooo8888.    888`88b.     888    '        `888.8'  `888.8'      888    '     888          888        ",
    r" 888          .8'     `888.   888  `88b.   888       o      `888'    `888'       888       o  888       o  888       o",
    r"o888o        o88o     o8888o o888o  o888o o888ooooood8       `8'      `8'       o888ooooood8 o888ooooood8 o888ooooood8",
];

const END_OF_LINE: &[&str] = &[
    r"oooooooooooo ooooo      ooo oooooooooo.          .oooooo.   oooooooooooo      ooooo        ooooo ooooo      ooo oooooooooooo",
    r"`888'     `8 `888b.     `8' `888'   `Y8b        d8P'  `Y8b  `888'     `8      `888'        `888' `888b.     `8' `888'     `8",
    r" 888          8 `88b.    8   888      888      888      888  888               888          888   8 `88b.    8   888        ",
    r" 888oooo8     8   `88b.  8   888      888      888      888  888oooo8          888          888   8   `88b.  8   888oooo8   ",
    r" 888    '     8     `88b.8   888      888      888      888  888    '          888          888   8     `88b.8   888    '   ",
    r" 888       o  8       `888   888     d88'      `88b    d88'  888               888       o  888   8       `888   888       o",
    r"o888ooooood8 o8o        `8  o888bood8P'         `Y8bood8P'  o888o             o888ooooood8 o888o o8o        `8  o888ooooood8",
];

const SAYONARA: &[&str] = &[
    r" .oooooo..o       .o.       oooooo   oooo   .oooooo.   ooooo      ooo       .o.       ooooooooo.         .o.      ",
    r"d8P'    `Y8      .888.       `888.   .8'   d8P'  `Y8b  `888b.     `8'      .888.      `888   `Y88.      .888.     ",
    r"Y88bo.          .8'888.       `888. .8'   888      888  8 `88b.    8      .8'888.      888   .d88'     .8'888.    ",
    r" `'Y8888o.     .8' `888.       `888.8'    888      888  8   `88b.  8     .8' `888.     888ooo88P'     .8' `888.   ",
    r"     `'Y88b   .88ooo8888.       `888'     888      888  8     `88b.8    .88ooo8888.    888`88b.      .88ooo8888.  ",
    r"oo     .d8P  .8'     `888.       888      `88b    d88'  8       `888   .8'     `888.   888  `88b.   .8'     `888. ",
    r"8''88888P'  o88o     o8888o     o888o      `Y8bood8P'  o8o        `8  o88o     o8888o o888o  o888o o88o     o8888o",
];

const TO_BE_CONTINUED: &[&str] = &[
    r"ooooooooooooo   .oooooo.        oooooooooo.  oooooooooooo",
    r"8'   888   `8  d8P'  `Y8b       `888'   `Y8b `888'     `8",
    r"     888      888      888       888     888  888        ",
    r"     888      888      888       888oooo888'  888oooo8   ",
    r"     888      888      888       888    `88b  888    '   ",
    r"     888      `88b    d88'       888    .88P  888       o",
    r"    o888o      `Y8bood8P'       o888bood8P'  o888ooooood8",
    r"",
    r"  .oooooo.     .oooooo.   ooooo      ooo ooooooooooooo ooooo ooooo      ooo ooooo     ooo oooooooooooo oooooooooo.   ",
    r" d8P'  `Y8b   d8P'  `Y8b  `888b.     `8' 8'   888   `8 `888' `888b.     `8' `888'     `8' `888'     `8 `888'   `Y8b  ",
    r"888          888      888  8 `88b.    8       888       888   8 `88b.    8   888       8   888          888      888 ",
    r"888          888      888  8   `88b.  8       888       888   8   `88b.  8   888       8   888oooo8     888      888 ",
    r"888          888      888  8     `88b.8       888       888   8     `88b.8   888       8   888    '     888      888 ",
    r"`88b    ooo  `88b    d88'  8       `888       888       888   8       `888   `88.    .8'   888       o  888     d88' ",
    r" `Y8bood8P'   `Y8bood8P'  o8o        `8      o888o     o888o o8o        `8     `YbodP'    o888ooooood8 o888bood8P'   ",
];

const BYE: &[&str] = &[
    r"oooooooooo.  oooooo   oooo oooooooooooo",
    r"`888'   `Y8b  `888.   .8'  `888'     `8",
    r" 888     888   `888. .8'    888        ",
    r" 888oooo888'    `888.8'     888oooo8   ",
    r" 888    `88b     `888'      888    '   ",
    r" 888    .88P      888       888       o",
    r"o888bood8P'      o888o     o888ooooood8",
];

const FIN: &[&str] = &[
    r"oooooooooooo ooooo ooooo      ooo",
    r"`888'     `8 `888' `888b.     `8'",
    r" 888          888   8 `88b.    8 ",
    r" 888oooo8     888   8   `88b.  8 ",
    r" 888    '     888   8     `88b.8 ",
    r" 888          888   8       `888 ",
    r"o888o        o888o o8o        `8 ",
];

const ADIOS: &[&str] = &[
    r"      .o.       oooooooooo.   ooooo   .oooooo.    .oooooo..o",
    r"     .888.      `888'   `Y8b  `888'  d8P'  `Y8b  d8P'    `Y8",
    r"    .8'888.      888      888  888  888      888 Y88bo.     ",
    r"   .8' `888.     888      888  888  888      888  `'Y8888o. ",
    r"  .88ooo8888.    888      888  888  888      888      `'Y88b",
    r" .8'     `888.   888     d88'  888  `88b    d88' oo     .d8P",
    r"o88o     o8888o o888bood8P'   o888o  `Y8bood8P'  8''88888P' ",
];

const MESSAGES: &[&[&str]] = &[
    FAREWELL,
    END_OF_LINE,
    SAYONARA,
    TO_BE_CONTINUED,
    BYE,
    FIN,
    ADIOS,
];

const SUBTITLE: &str = "Borg Decommissioned";

pub fn render(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    let idx = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as usize % MESSAGES.len())
        .unwrap_or(0);
    let art = MESSAGES[idx];

    let style = Style::default().fg(theme::CYAN);
    let dim = Style::default().fg(theme::DIM_WHITE);

    terminal.draw(|f| {
        let area = f.area();
        f.render_widget(ratatui::widgets::Clear, area);

        let art_height = art.len() as u16;
        let art_width = art.iter().map(|l| l.len()).max().unwrap_or(0) as u16;
        let total_height = art_height + 2;

        // Center vertically
        let v_pad = area.height.saturating_sub(total_height) / 2;
        let chunks = Layout::vertical([
            Constraint::Length(v_pad),
            Constraint::Length(art_height),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

        // Center art horizontally
        let h_pad = area.width.saturating_sub(art_width) / 2;
        let h_chunks = Layout::horizontal([
            Constraint::Length(h_pad),
            Constraint::Length(art_width),
            Constraint::Min(0),
        ])
        .split(chunks[1]);

        let lines: Vec<Line> = art
            .iter()
            .map(|l| Line::from(Span::styled(*l, style)))
            .collect();
        f.render_widget(Paragraph::new(lines), h_chunks[1]);

        // Center subtitle
        let sub_len = SUBTITLE.len() as u16;
        let sub_pad = area.width.saturating_sub(sub_len) / 2;
        let sub_chunks = Layout::horizontal([
            Constraint::Length(sub_pad),
            Constraint::Length(sub_len),
            Constraint::Min(0),
        ])
        .split(chunks[3]);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(SUBTITLE, dim))),
            sub_chunks[1],
        );
    })?;

    Ok(())
}

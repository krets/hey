//! Response rendering (spec section 9).
//!
//! - TTY, no colour: text passes straight through, token by token.
//! - Not a TTY: fence lines (```) are dropped so the output is pipeable.
//! - TTY with colour enabled: fenced code is highlighted.

use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plain,
    Strip,
    Colour,
}

impl Mode {
    pub fn detect(is_tty: bool, color_enabled: bool, no_color_env: bool) -> Mode {
        if !is_tty {
            Mode::Strip
        } else if color_enabled && !no_color_env {
            Mode::Colour
        } else {
            Mode::Plain
        }
    }
}

pub struct Renderer<W: Write> {
    out: W,
    mode: Mode,
    line: String,
    in_fence: bool,
    wrote_any: bool,
    last_was_newline: bool,
}

const CODE: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn is_fence(line: &str) -> bool {
    match line.trim().strip_prefix("```") {
        Some(lang) => !lang.contains(char::is_whitespace) && !lang.contains('`'),
        None => false,
    }
}

impl<W: Write> Renderer<W> {
    pub fn new(out: W, mode: Mode) -> Self {
        Renderer {
            out,
            mode,
            line: String::new(),
            in_fence: false,
            wrote_any: false,
            last_was_newline: true,
        }
    }

    pub fn push(&mut self, text: &str) -> std::io::Result<()> {
        if self.mode == Mode::Plain {
            return self.emit(text);
        }
        self.line.push_str(text);
        while let Some(i) = self.line.find('\n') {
            let full: String = self.line.drain(..=i).collect();
            self.line_out(full.trim_end_matches(['\n', '\r']), true)?;
        }
        Ok(())
    }

    /// Flush a partial last line and make sure the output ends in a newline.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if !self.line.is_empty() {
            let rest = std::mem::take(&mut self.line);
            self.line_out(&rest, false)?;
        }
        if self.wrote_any && !self.last_was_newline {
            self.emit("\n")?;
        }
        self.out.flush()
    }

    fn line_out(&mut self, line: &str, newline: bool) -> std::io::Result<()> {
        let nl = if newline { "\n" } else { "" };
        if is_fence(line) {
            self.in_fence = !self.in_fence;
            return match self.mode {
                Mode::Strip => Ok(()),
                _ => self.emit(&format!("{DIM}{line}{RESET}{nl}")),
            };
        }
        if self.mode == Mode::Colour && self.in_fence {
            self.emit(&format!("{CODE}{line}{RESET}{nl}"))
        } else {
            self.emit(&format!("{line}{nl}"))
        }
    }

    fn emit(&mut self, s: &str) -> std::io::Result<()> {
        if s.is_empty() {
            return Ok(());
        }
        self.out.write_all(s.as_bytes())?;
        // Streaming wants tokens on screen now.
        self.out.flush()?;
        self.wrote_any = true;
        self.last_was_newline = s.ends_with('\n');
        Ok(())
    }
}

/// Render a complete response in one go.
pub fn render_all<W: Write>(out: W, mode: Mode, text: &str) -> std::io::Result<()> {
    let mut r = Renderer::new(out, mode);
    r.push(text.trim())?;
    r.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mode: Mode, text: &str) -> String {
        let mut buf = Vec::new();
        render_all(&mut buf, mode, text).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn strips_fences_when_piped() {
        assert_eq!(run(Mode::Strip, "```bash\ngit push -u origin main\n```"), "git push -u origin main\n");
        assert_eq!(run(Mode::Strip, "Try:\n```\nls -la\n```\nDone."), "Try:\nls -la\nDone.\n");
    }

    #[test]
    fn keeps_non_fence_backticks() {
        assert_eq!(run(Mode::Strip, "use `ls` here"), "use `ls` here\n");
        assert_eq!(run(Mode::Strip, "```not a fence```"), "```not a fence```\n");
    }

    #[test]
    fn plain_tty_output_is_verbatim() {
        assert_eq!(run(Mode::Plain, "```sh\nls\n```"), "```sh\nls\n```\n");
    }

    #[test]
    fn colour_highlights_code_only() {
        let out = run(Mode::Colour, "hi\n```\nls\n```\nbye");
        assert!(out.contains("\x1b[36mls\x1b[0m\n"));
        assert!(out.starts_with("hi\n"));
        assert!(out.ends_with("bye\n"));
    }

    #[test]
    fn streaming_deltas_split_mid_line_and_mid_fence() {
        let mut buf = Vec::new();
        {
            let mut r = Renderer::new(&mut buf, Mode::Strip);
            for piece in ["``", "`sh\nls -", "la\n`", "``\n"] {
                r.push(piece).unwrap();
            }
            r.finish().unwrap();
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "ls -la\n");
    }

    #[test]
    fn always_ends_with_one_newline() {
        assert_eq!(run(Mode::Plain, "abc\n\n"), "abc\n");
        assert_eq!(run(Mode::Strip, ""), "");
    }

    #[test]
    fn mode_detection() {
        assert_eq!(Mode::detect(false, true, false), Mode::Strip);
        assert_eq!(Mode::detect(true, false, false), Mode::Plain);
        assert_eq!(Mode::detect(true, true, true), Mode::Plain);
        assert_eq!(Mode::detect(true, true, false), Mode::Colour);
    }
}

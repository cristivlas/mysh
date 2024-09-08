use super::{register_command, Exec, ShellCommand};
use crate::symlnk::SymLink;
use crate::utils::format_error;
use crate::{cmds::flags::CommandFlags, eval::Value, scope::Scope};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    style::Print,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
    QueueableCommand,
};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::rc::Rc;
use terminal_size::{terminal_size, Height, Width};

struct LessViewer {
    lines: Vec<String>,
    current_line: usize,
    horizontal_scroll: usize,
    screen_width: usize,
    screen_height: usize,
    last_search: Option<String>,
    last_search_direction: bool,
    line_num_width: usize,
    search_start_index: usize,
    show_line_numbers: bool,
    status: Option<String>,
    use_color: bool,
}

impl LessViewer {
    fn new<R: BufRead>(reader: R) -> io::Result<Self> {
        let lines: Vec<String> = reader.lines().collect::<io::Result<_>>()?;
        let (Width(w), Height(h)) = terminal_size().unwrap_or((Width(80), Height(24)));

        Ok(LessViewer {
            line_num_width: lines.len().to_string().len() + 1,
            lines,
            current_line: 0,
            horizontal_scroll: 0,
            screen_width: w as usize,
            screen_height: h.saturating_sub(1) as usize,
            last_search: None,
            last_search_direction: true,
            search_start_index: 0,
            show_line_numbers: false,
            status: None,
            use_color: true,
        })
    }

    fn display_page(&self, stdout: &mut std::io::Stdout, buffer: &mut String) -> io::Result<()> {
        buffer.clear();
        buffer.push('\n');

        let end = (self.current_line + self.screen_height).min(self.lines.len());
        for (index, line) in self.lines[self.current_line..end].iter().enumerate() {
            if self.show_line_numbers {
                let line_number = self.current_line + index + 1;
                buffer.push_str(&format!("{:>w$}", line_number, w = self.line_num_width));
            }
            self.display_line(line, buffer)?;
        }

        // Fill any remaining lines with empty space
        for _ in end..self.current_line + self.screen_height {
            if self.show_line_numbers {
                buffer.push_str(&" ".repeat(self.screen_width.saturating_sub(1)));
            }
            buffer.push('\n');
        }

        if let Some(ref message) = self.status {
            buffer.push_str(message);
        } else {
            buffer.push(':');
        }

        print!("{}", buffer);
        stdout.flush()?;

        Ok(())
    }

    fn display_line(&self, line: &str, buffer: &mut String) -> io::Result<()> {
        let displayed = if line.len() > self.horizontal_scroll {
            &line[self.horizontal_scroll..]
        } else {
            ""
        };
        let displayed = &displayed[..displayed.len().min(self.screen_width)];

        if self.show_line_numbers {
            buffer.push_str("  ");
        }

        if let Some(ref search) = self.last_search {
            let mut start = 0;
            while let Some(index) = displayed[start..].find(search) {
                let end = start + index + search.len();
                buffer.push_str(&displayed[start..start + index]);
                if self.use_color {
                    buffer.push_str("\x1b[43m\x1b[30m");
                }
                buffer.push_str(&displayed[start + index..end]);
                if self.use_color {
                    buffer.push_str("\x1b[0m");
                }
                start = end;
            }
            buffer.push_str(&displayed[start..]);
        } else {
            buffer.push_str(displayed);
        }

        buffer.push('\n');
        Ok(())
    }

    fn last_page(&mut self) {
        if self.lines.is_empty() {
            self.current_line = 0;
        } else {
            self.current_line = self.lines.len().saturating_sub(self.screen_height);
        }
    }

    fn next_line(&mut self) {
        if self.current_line < self.lines.len() - 1 {
            self.current_line += 1;
            if self.current_line + self.screen_height > self.lines.len() {
                self.current_line = self.lines.len().saturating_sub(self.screen_height);
            }
        }
    }

    fn next_page(&mut self) {
        let new_line =
            (self.current_line + self.screen_height).min(self.lines.len().saturating_sub(1));
        if new_line > self.current_line {
            self.current_line = new_line;
            if self.current_line + self.screen_height > self.lines.len() {
                self.current_line = self.lines.len().saturating_sub(self.screen_height);
            }
        }
    }

    fn prev_page(&mut self) {
        self.current_line = self.current_line.saturating_sub(self.screen_height);
    }

    fn prev_line(&mut self) {
        if self.current_line > 0 {
            self.current_line -= 1;
        }
    }

    fn scroll_right(&mut self) {
        self.horizontal_scroll += 1;
    }

    fn scroll_left(&mut self) {
        self.horizontal_scroll = self.horizontal_scroll.saturating_sub(1);
    }

    fn search(&mut self, query: &str, forward: bool) {
        self.last_search = Some(query.to_string());
        self.last_search_direction = forward;

        let mut found = false;

        if forward {
            for (index, line) in self.lines[self.search_start_index..].iter().enumerate() {
                if line.contains(query) {
                    self.current_line = self.search_start_index + index;
                    self.search_start_index = self.current_line + 1;
                    found = true;
                    break;
                }
            }
        } else {
            for (index, line) in self.lines[..self.search_start_index]
                .iter()
                .rev()
                .enumerate()
            {
                if line.contains(query) {
                    self.current_line = self.search_start_index - index - 1;
                    self.search_start_index = self.current_line;
                    found = true;
                    break;
                }
            }
        }

        if !found {
            self.status = Some(format!("Pattern not found: {}", query));
            self.search_start_index = if forward {
                self.current_line + 1
            } else {
                self.current_line
            };
        }

        if self.current_line + self.screen_height > self.lines.len() {
            self.current_line = self.lines.len().saturating_sub(self.screen_height);
        }
    }

    fn repeat_search(&mut self) {
        if let Some(query) = self.last_search.clone() {
            let direction = self.last_search_direction;
            self.search(&query, direction);
        }
    }

    fn run(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;

        let mut buffer = String::with_capacity(self.screen_width * self.screen_height);

        self.display_page(&mut stdout, &mut buffer)?;

        loop {
            let (mut current_line, horizontal_scroll, search_term, show_lines) = (
                self.current_line,
                self.horizontal_scroll,
                self.last_search.clone(),
                self.show_line_numbers,
            );
            self.status = None;

            if let Event::Key(key_event) = event::read()? {
                if key_event.kind != KeyEventKind::Press {
                    continue;
                }

                match key_event.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('b') => self.prev_page(),
                    KeyCode::Char('f') => self.next_page(),
                    KeyCode::Char(' ') => self.next_page(),
                    KeyCode::Char('G') => self.last_page(),
                    KeyCode::Enter => self.next_line(),
                    KeyCode::Up => self.prev_line(),
                    KeyCode::Down => self.next_line(),
                    KeyCode::Left => self.scroll_left(),
                    KeyCode::Right => self.scroll_right(),
                    KeyCode::PageUp => self.prev_page(),
                    KeyCode::PageDown => self.next_page(),
                    KeyCode::Char('/') | KeyCode::Char('?') => {
                        execute!(
                            stdout,
                            cursor::SavePosition,
                            cursor::MoveTo(0, self.screen_height as u16),
                            Clear(ClearType::CurrentLine),
                            cursor::RestorePosition
                        )?;

                        let prompt_char = if key_event.code == KeyCode::Char('/') {
                            '/'
                        } else {
                            '?'
                        };
                        let query = self.prompt_for_query(prompt_char)?;
                        if query.is_empty() {
                            self.status = None;
                            current_line = usize::MAX;
                        } else {
                            self.search(&query, prompt_char == '/');
                        }
                    }
                    KeyCode::Char('n') => {
                        self.repeat_search();
                    }
                    KeyCode::Char('l') => {
                        self.show_line_numbers = !self.show_line_numbers;
                    }
                    _ => {}
                }

                if current_line != self.current_line
                    || horizontal_scroll != self.horizontal_scroll
                    || search_term != self.last_search
                    || show_lines != self.show_line_numbers
                {
                    self.display_page(&mut stdout, &mut buffer)?;
                }
            }
        }

        execute!(stdout, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    }

    fn prompt_for_query(&mut self, prompt_char: char) -> io::Result<String> {
        let mut stdout = io::stdout();
        stdout
            .queue(cursor::SavePosition)?
            .queue(cursor::MoveTo(0, self.screen_height as u16))?
            .queue(Print(prompt_char.to_string()))?
            .flush()?;

        let query = crate::prompt::read_input("Search: ")?;

        stdout.queue(cursor::RestorePosition)?.flush()?;
        Ok(query.trim().to_string())
    }
}

struct Less {
    flags: CommandFlags,
}

impl Less {
    fn new() -> Self {
        let mut flags = CommandFlags::new();
        flags.add_flag('?', "help", "Display this help message");
        flags.add_flag('n', "number", "Number output lines");
        Less { flags }
    }
}

impl Exec for Less {
    fn exec(&self, name: &str, args: &Vec<String>, scope: &Rc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        let filenames = flags.parse(scope, args)?;

        if flags.is_present("help") {
            println!("Usage: {} [OPTION]... [FILE]...", name);
            println!("View FILE(s) or standard input in a pager.");
            println!("\nOptions:");
            print!("{}", flags.help());
            return Ok(Value::success());
        }

        if filenames.is_empty() {
            let stdin = io::stdin();
            let reader = stdin.lock();
            run_less_viewer(scope, &flags, reader).map_err(|e| e.to_string())?;
        } else {
            for filename in &filenames {
                let path = Path::new(filename)
                    .resolve()
                    .map_err(|e| format_error(&scope, filename, args, e))?;

                let file =
                    File::open(&path).map_err(|e| format_error(&scope, filename, args, e))?;
                let reader = BufReader::new(file);
                run_less_viewer(scope, &flags, reader).map_err(|e| e.to_string())?;
            }
        };

        Ok(Value::success())
    }
}

fn run_less_viewer<R: BufRead>(
    scope: &Rc<Scope>,
    flags: &CommandFlags,
    reader: R,
) -> io::Result<()> {
    let mut viewer = LessViewer::new(reader)?;

    viewer.show_line_numbers = flags.is_present("number");
    viewer.use_color = scope.use_colors(&std::io::stdout());

    viewer.run()
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "less".to_string(),
        inner: Rc::new(Less::new()),
    });
}

use anyhow::Result;
use helix_core::Position;
use helix_view::tree::Layout;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Args {
    pub display_help: bool,
    pub display_version: bool,
    pub health: bool,
    pub health_arg: Option<String>,
    pub load_tutor: bool,
    pub fetch_grammars: bool,
    pub build_grammars: bool,
    pub strict: bool,
    pub split: Option<Layout>,
    pub verbosity: u64,
    pub log_file: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub files: IndexMap<PathBuf, Vec<Position>>,
    pub working_directory: Option<PathBuf>,
    /// Join the editor shared under this name.
    pub session: Option<String>,
    pub no_session: bool,
    /// Run the shared editor for this session key. Started by the first client.
    pub session_server: Option<String>,
    /// Open the files in the session's focused client rather than this terminal.
    pub remote: bool,
    /// With `remote`, wait until none of the files is shown.
    pub wait: bool,
    /// Save this client's buffers this many milliseconds after an edit, and when its terminal
    /// loses focus.
    pub auto_save: Option<u64>,
}

impl Args {
    pub fn parse_args() -> Result<Args> {
        Self::parse(std::env::args())
    }

    /// Parses a command line, starting with the program name. Relative paths resolve against
    /// [`helix_stdx::env::current_working_dir`].
    #[allow(clippy::too_many_lines)]
    pub fn parse(argv: impl IntoIterator<Item = String>) -> Result<Args> {
        let mut args = Args::default();
        let mut argv = argv.into_iter().peekable();
        let mut line_number = 0;

        let mut insert_file_with_position = |file_with_position: &str| {
            let (filename, position) = parse_file(file_with_position);

            // Before setting the working directory, resolve all the paths in args.files
            let filename = helix_stdx::path::canonicalize(filename);

            args.files
                .entry(filename)
                .and_modify(|positions| positions.push(position))
                .or_insert_with(|| vec![position]);
        };

        argv.next(); // skip the program, we don't care about that

        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--" => break, // stop parsing at this point treat the remaining as files
                "--version" => args.display_version = true,
                "--help" => args.display_help = true,
                "--strict" => args.strict = true,
                "--tutor" => args.load_tutor = true,
                "--vsplit" => match args.split {
                    Some(_) => anyhow::bail!("can only set a split once of a specific type"),
                    None => args.split = Some(Layout::Vertical),
                },
                "--hsplit" => match args.split {
                    Some(_) => anyhow::bail!("can only set a split once of a specific type"),
                    None => args.split = Some(Layout::Horizontal),
                },
                "--health" => {
                    args.health = true;
                    args.health_arg = argv.next_if(|opt| !opt.starts_with('-'));
                }
                "-g" | "--grammar" => match argv.next().as_deref() {
                    Some("fetch") => args.fetch_grammars = true,
                    Some("build") => args.build_grammars = true,
                    _ => {
                        anyhow::bail!("--grammar must be followed by either 'fetch' or 'build'")
                    }
                },
                "-c" | "--config" => match argv.next().as_deref() {
                    Some(path) => args.config_file = Some(path.into()),
                    None => anyhow::bail!("--config must specify a path to read"),
                },
                "-s" | "--session" => match argv.next() {
                    Some(name) => args.session = Some(name),
                    None => anyhow::bail!("--session must specify a session name"),
                },
                "--no-session" => args.no_session = true,
                "--remote" => args.remote = true,
                "--wait" => args.wait = true,
                "--auto-save" => match argv.next().map(|ms| ms.parse()) {
                    Some(Ok(ms)) => args.auto_save = Some(ms),
                    _ => anyhow::bail!("--auto-save must specify a delay in milliseconds"),
                },
                "--session-server" => match argv.next() {
                    Some(key) => args.session_server = Some(key),
                    None => anyhow::bail!("--session-server must specify a session key"),
                },
                "--log" => match argv.next().as_deref() {
                    Some(path) => args.log_file = Some(path.into()),
                    None => anyhow::bail!("--log must specify a path to write"),
                },
                "-w" | "--working-dir" => match argv.next().as_deref() {
                    Some(path) => {
                        args.working_directory = if Path::new(path).is_dir() {
                            Some(PathBuf::from(path))
                        } else {
                            anyhow::bail!(
                                "--working-dir specified does not exist or is not a directory"
                            )
                        }
                    }
                    None => {
                        anyhow::bail!("--working-dir must specify an initial working directory")
                    }
                },
                arg if arg.starts_with("--") => {
                    anyhow::bail!("unexpected double dash argument: {}", arg)
                }
                arg if arg.starts_with('-') => {
                    let arg = arg.get(1..).unwrap().chars();
                    for chr in arg {
                        match chr {
                            'v' => args.verbosity += 1,
                            'V' => args.display_version = true,
                            'h' => args.display_help = true,
                            _ => anyhow::bail!("unexpected short arg {}", chr),
                        }
                    }
                }
                "+" => line_number = usize::MAX,
                arg if arg.starts_with('+') => {
                    match arg[1..].parse::<usize>() {
                        Ok(n) => line_number = n.saturating_sub(1),
                        _ => insert_file_with_position(arg),
                    };
                }
                arg => insert_file_with_position(arg),
            }
        }

        // push the remaining args, if any to the files
        for arg in argv {
            insert_file_with_position(&arg);
        }

        if line_number != 0 {
            if let Some(first_position) = args
                .files
                .first_mut()
                .and_then(|(_, positions)| positions.first_mut())
            {
                first_position.row = line_number;
            }
        }

        Ok(args)
    }
}

/// Parse arg into [`PathBuf`] and position.
pub(crate) fn parse_file(s: &str) -> (PathBuf, Position) {
    let def = || (PathBuf::from(s), Position::default());
    if Path::new(s).exists() {
        return def();
    }
    split_path_row_col(s)
        .or_else(|| split_path_row(s))
        .unwrap_or_else(def)
}

/// Split file.rs:10:2 into [`PathBuf`], row and col.
///
/// Does not validate if file.rs is a file or directory.
fn split_path_row_col(s: &str) -> Option<(PathBuf, Position)> {
    let mut s = s.trim_end_matches(':').rsplitn(3, ':');
    let col: usize = s.next()?.parse().ok()?;
    let row: usize = s.next()?.parse().ok()?;
    let path = s.next()?.into();
    let pos = Position::new(row.saturating_sub(1), col.saturating_sub(1));
    Some((path, pos))
}

/// Split file.rs:10 into [`PathBuf`] and row.
///
/// Does not validate if file.rs is a file or directory.
fn split_path_row(s: &str) -> Option<(PathBuf, Position)> {
    let (path, row) = s.trim_end_matches(':').rsplit_once(':')?;
    let row: usize = row.parse().ok()?;
    let path = path.into();
    let pos = Position::new(row.saturating_sub(1), 0);
    Some((path, pos))
}

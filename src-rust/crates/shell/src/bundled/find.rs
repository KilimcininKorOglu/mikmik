//! `find`, running in this process.
//!
//! `findutils` takes the writer its output goes to, so unlike the coreutils
//! this one needs no child process at all: it writes straight into whatever
//! the shell redirected the command to.

use std::cell::RefCell;
use std::io::Write;
use std::time::SystemTime;

use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::commands::{CommandArg, ExecutionContext};
use brush_core::extensions::ShellExtensions;

/// Where `find` writes and what it is told about the world.
struct Deps {
    output: RefCell<Box<dyn Write>>,
}

impl Deps {
    /// Send `find`'s output to `writer`.
    fn writing_to(writer: impl Write + 'static) -> Self {
        Self {
            output: RefCell::new(Box::new(writer)),
        }
    }
}

impl findutils::find::Dependencies for Deps {
    fn get_output(&self) -> &RefCell<dyn Write> {
        // The `RefCell<Box<dyn Write>>` is what the field can hold; the trait
        // asks for `RefCell<dyn Write>`, and a `Box<dyn Write>` is itself a
        // `Write`, so the coercion is the whole conversion.
        &self.output
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn confirm(&self, _prompt: &str) -> bool {
        // `-ok` asks before acting on each file. There is nobody at a keyboard
        // in a session the model drives, and a silent yes would delete things
        // nobody agreed to, so the answer is always no.
        false
    }
}

pub(super) fn registration<SE: ShellExtensions>() -> Registration<SE> {
    Registration {
        execute_func: execute::<SE>,
        content_func: content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
}

fn execute<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<brush_core::ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let owned: Vec<String> = args
            .iter()
            .map(|arg| match arg {
                CommandArg::String(value) => value.clone(),
                other => other.to_string(),
            })
            .collect();
        let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();

        let deps = Deps::writing_to(context.stdout());
        let code = findutils::find::find_main(&borrowed, &deps);
        // Flushed here rather than left to the drop: the writer is a handle on
        // the shell's own descriptor, and a buffered tail that arrives after
        // the next command has started prints in the wrong place.
        let _ = deps.output.borrow_mut().flush();

        Ok(brush_core::ExecutionResult::new(
            u8::try_from(code).unwrap_or(1),
        ))
    })
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "the signature is `brush_core::builtins::CommandContentFunc`"
)]
fn content(
    name: &str,
    content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    match content_type {
        ContentType::ShortDescription => Ok(format!("{name} - search a directory tree")),
        ContentType::DetailedHelp => Ok(format!("{name} - search a directory tree\n")),
        ContentType::ShortUsage | ContentType::ManPage => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer the test can read back, standing in for the shell's stdout.
    #[derive(Clone, Default)]
    struct Collected(std::rc::Rc<RefCell<Vec<u8>>>);

    impl Write for Collected {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `find` over a tree and answer what it wrote.
    fn run(args: &[&str]) -> (String, i32) {
        let collected = Collected::default();
        let deps = Deps::writing_to(collected.clone());
        let code = findutils::find::find_main(args, &deps);
        drop(deps);
        let bytes = collected.0.borrow().clone();
        (String::from_utf8_lossy(&bytes).into_owned(), code)
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/main.rs"), "").expect("write");
        std::fs::write(dir.path().join("src/lib.rs"), "").expect("write");
        std::fs::write(dir.path().join("notes.txt"), "").expect("write");
        dir
    }

    #[test]
    fn it_writes_into_the_writer_it_was_given() {
        // The whole reason `find` runs in this process rather than as a child:
        // it takes the writer, so the shell's own redirection applies with no
        // process in between.
        let dir = tree();
        let root = dir.path().display().to_string();
        let (output, code) = run(&["find", &root, "-name", "*.rs"]);

        assert_eq!(code, 0);
        assert!(output.contains("main.rs"), "{output}");
        assert!(output.contains("lib.rs"), "{output}");
        assert!(!output.contains("notes.txt"), "{output}");
    }

    #[test]
    fn a_type_filter_is_honoured() {
        let dir = tree();
        let root = dir.path().display().to_string();
        let (files, _) = run(&["find", &root, "-type", "f"]);
        let (dirs, _) = run(&["find", &root, "-type", "d"]);

        assert!(files.contains("notes.txt"), "{files}");
        assert!(!files.lines().any(|line| line.ends_with("/src")), "{files}");
        assert!(dirs.lines().any(|line| line.ends_with("/src")), "{dirs}");
    }

    #[test]
    fn a_prompt_nobody_can_answer_is_refused() {
        // `-ok` asks before acting on each file. There is nobody at a keyboard
        // in a session the model drives, and a silent yes would delete things
        // nobody agreed to.
        let deps = Deps::writing_to(Vec::new());
        assert!(!findutils::find::Dependencies::confirm(
            &deps,
            "remove everything?"
        ));
    }
}

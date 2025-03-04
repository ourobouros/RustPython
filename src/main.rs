#[macro_use]
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate log;

use clap::{App, AppSettings, Arg, ArgMatches};
use rustpython_compiler::{compile, error::CompileError, error::CompileErrorType};
use rustpython_parser::error::ParseErrorType;
use rustpython_vm::{
    import,
    obj::objstr::PyStringRef,
    print_exception,
    pyobject::{ItemProtocol, PyObjectRef, PyResult},
    scope::Scope,
    util, PySettings, VirtualMachine,
};
use std::convert::TryInto;

use std::env;
use std::path::PathBuf;
use std::process;
use std::str::FromStr;

fn main() {
    #[cfg(feature = "flame-it")]
    let main_guard = flame::start_guard("RustPython main");
    env_logger::init();
    let app = App::new("RustPython");
    let matches = parse_arguments(app);
    let settings = create_settings(&matches);
    let vm = VirtualMachine::new(settings);

    let res = run_rustpython(&vm, &matches);
    // See if any exception leaked out:
    handle_exception(&vm, res);

    #[cfg(feature = "flame-it")]
    {
        main_guard.end();
        if let Err(e) = write_profile(&matches) {
            error!("Error writing profile information: {}", e);
            process::exit(1);
        }
    }
}

fn parse_arguments<'a>(app: App<'a, '_>) -> ArgMatches<'a> {
    let app = app
        .setting(AppSettings::TrailingVarArg)
        .version(crate_version!())
        .author(crate_authors!())
        .about("Rust implementation of the Python language")
        .usage("rustpython [OPTIONS] [-c CMD | -m MODULE | FILE] [PYARGS]...")
        .arg(
            Arg::with_name("script")
                .required(false)
                .allow_hyphen_values(true)
                .multiple(true)
                .value_name("script, args")
                .min_values(1),
        )
        .arg(
            Arg::with_name("c")
                .short("c")
                .takes_value(true)
                .allow_hyphen_values(true)
                .multiple(true)
                .value_name("cmd, args")
                .min_values(1)
                .help("run the given string as a program"),
        )
        .arg(
            Arg::with_name("m")
                .short("m")
                .takes_value(true)
                .allow_hyphen_values(true)
                .multiple(true)
                .value_name("module, args")
                .min_values(1)
                .help("run library module as script"),
        )
        .arg(
            Arg::with_name("optimize")
                .short("O")
                .multiple(true)
                .help("Optimize. Set __debug__ to false. Remove debug statements."),
        )
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .multiple(true)
                .help("Give the verbosity (can be applied multiple times)"),
        )
        .arg(Arg::with_name("debug").short("d").help("Debug the parser."))
        .arg(
            Arg::with_name("quiet")
                .short("q")
                .help("Be quiet at startup."),
        )
        .arg(
            Arg::with_name("inspect")
                .short("i")
                .help("Inspect interactively after running the script."),
        )
        .arg(
            Arg::with_name("no-user-site")
                .short("s")
                .help("don't add user site directory to sys.path."),
        )
        .arg(
            Arg::with_name("no-site")
                .short("S")
                .help("don't imply 'import site' on initialization"),
        )
        .arg(
            Arg::with_name("dont-write-bytecode")
                .short("B")
                .help("don't write .pyc files on import"),
        )
        .arg(
            Arg::with_name("ignore-environment")
                .short("E")
                .help("Ignore environment variables PYTHON* such as PYTHONPATH"),
        );
    #[cfg(feature = "flame-it")]
    let app = app
        .arg(
            Arg::with_name("profile_output")
                .long("profile-output")
                .takes_value(true)
                .help("the file to output the profiling information to"),
        )
        .arg(
            Arg::with_name("profile_format")
                .long("profile-format")
                .takes_value(true)
                .help("the profile format to output the profiling information in"),
        );
    app.get_matches()
}

/// Create settings by examining command line arguments and environment
/// variables.
fn create_settings(matches: &ArgMatches) -> PySettings {
    let ignore_environment = matches.is_present("ignore-environment");
    let mut settings: PySettings = Default::default();
    settings.ignore_environment = ignore_environment;

    // add the current directory to sys.path
    settings.path_list.push("".to_owned());

    if !ignore_environment {
        settings.path_list.append(&mut get_paths("RUSTPYTHONPATH"));
        settings.path_list.append(&mut get_paths("PYTHONPATH"));
    }

    // Now process command line flags:
    if matches.is_present("debug") || (!ignore_environment && env::var_os("PYTHONDEBUG").is_some())
    {
        settings.debug = true;
    }

    if matches.is_present("inspect")
        || (!ignore_environment && env::var_os("PYTHONINSPECT").is_some())
    {
        settings.inspect = true;
    }

    if matches.is_present("optimize") {
        settings.optimize = matches.occurrences_of("optimize").try_into().unwrap();
    } else if !ignore_environment {
        if let Ok(value) = get_env_var_value("PYTHONOPTIMIZE") {
            settings.optimize = value;
        }
    }

    if matches.is_present("verbose") {
        settings.verbose = matches.occurrences_of("verbose").try_into().unwrap();
    } else if !ignore_environment {
        if let Ok(value) = get_env_var_value("PYTHONVERBOSE") {
            settings.verbose = value;
        }
    }

    settings.no_site = matches.is_present("no-site");

    if matches.is_present("no-user-site")
        || (!ignore_environment && env::var_os("PYTHONNOUSERSITE").is_some())
    {
        settings.no_user_site = true;
    }

    if matches.is_present("quiet") {
        settings.quiet = true;
    }

    if matches.is_present("dont-write-bytecode")
        || (!ignore_environment && env::var_os("PYTHONDONTWRITEBYTECODE").is_some())
    {
        settings.dont_write_bytecode = true;
    }

    let argv = if let Some(script) = matches.values_of("script") {
        script.map(ToOwned::to_owned).collect()
    } else if let Some(module) = matches.values_of("m") {
        std::iter::once("PLACEHOLDER".to_owned())
            .chain(module.skip(1).map(ToOwned::to_owned))
            .collect()
    } else if let Some(cmd) = matches.values_of("c") {
        std::iter::once("-c".to_owned())
            .chain(cmd.skip(1).map(ToOwned::to_owned))
            .collect()
    } else {
        vec![]
    };

    settings.argv = argv;

    settings
}

/// Get environment variable and turn it into integer.
fn get_env_var_value(name: &str) -> Result<u8, std::env::VarError> {
    env::var(name).map(|value| {
        if let Ok(value) = u8::from_str(&value) {
            value
        } else {
            1
        }
    })
}

/// Helper function to retrieve a sequence of paths from an environment variable.
fn get_paths(env_variable_name: &str) -> Vec<String> {
    let paths = env::var_os(env_variable_name);
    match paths {
        Some(paths) => env::split_paths(&paths)
            .map(|path| {
                path.into_os_string()
                    .into_string()
                    .unwrap_or_else(|_| panic!("{} isn't valid unicode", env_variable_name))
            })
            .collect(),
        None => vec![],
    }
}

#[cfg(feature = "flame-it")]
fn write_profile(matches: &ArgMatches) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;

    enum ProfileFormat {
        Html,
        Text,
        Speedscope,
    }

    let profile_output = matches.value_of_os("profile_output");

    let profile_format = match matches.value_of("profile_format") {
        Some("html") => ProfileFormat::Html,
        Some("text") => ProfileFormat::Text,
        None if profile_output == Some("-".as_ref()) => ProfileFormat::Text,
        Some("speedscope") | None => ProfileFormat::Speedscope,
        Some(other) => {
            error!("Unknown profile format {}", other);
            process::exit(1);
        }
    };

    let profile_output = profile_output.unwrap_or_else(|| match profile_format {
        ProfileFormat::Html => "flame-graph.html".as_ref(),
        ProfileFormat::Text => "flame.txt".as_ref(),
        ProfileFormat::Speedscope => "flamescope.json".as_ref(),
    });

    let profile_output: Box<dyn std::io::Write> = if profile_output == "-" {
        Box::new(std::io::stdout())
    } else {
        Box::new(File::create(profile_output)?)
    };

    match profile_format {
        ProfileFormat::Html => flame::dump_html(profile_output)?,
        ProfileFormat::Text => flame::dump_text_to_writer(profile_output)?,
        ProfileFormat::Speedscope => flamescope::dump(profile_output)?,
    }

    Ok(())
}

fn run_rustpython(vm: &VirtualMachine, matches: &ArgMatches) -> PyResult<()> {
    import::init_importlib(&vm, true)?;

    if let Some(paths) = option_env!("BUILDTIME_RUSTPYTHONPATH") {
        let sys_path = vm.get_attribute(vm.sys_module.clone(), "path")?;
        for (i, path) in std::env::split_paths(paths).enumerate() {
            vm.call_method(
                &sys_path,
                "insert",
                vec![
                    vm.ctx.new_int(i),
                    vm.ctx.new_str(
                        path.into_os_string()
                            .into_string()
                            .expect("Invalid UTF8 in BUILDTIME_RUSTPYTHONPATH"),
                    ),
                ],
            )?;
        }
    }

    let scope = vm.new_scope_with_builtins();
    let main_module = vm.new_module("__main__", scope.globals.clone());

    vm.get_attribute(vm.sys_module.clone(), "modules")?
        .set_item("__main__", main_module, vm)?;

    let site_result = vm.import("site", &[], 0);

    if site_result.is_err() {
        warn!(
            "Failed to import site, consider adding the Lib directory to your RUSTPYTHONPATH \
             environment variable",
        );
    }

    // Figure out if a -c option was given:
    if let Some(command) = matches.value_of("c") {
        run_command(&vm, scope, command.to_string())?;
    } else if let Some(module) = matches.value_of("m") {
        run_module(&vm, module)?;
    } else if let Some(filename) = matches.value_of("script") {
        run_script(&vm, scope, filename)?
    } else {
        run_shell(&vm, scope)?;
    }

    Ok(())
}

fn _run_string(vm: &VirtualMachine, scope: Scope, source: &str, source_path: String) -> PyResult {
    let code_obj = vm
        .compile(source, compile::Mode::Exec, source_path.clone())
        .map_err(|err| vm.new_syntax_error(&err))?;
    // trace!("Code object: {:?}", code_obj.borrow());
    scope
        .globals
        .set_item("__file__", vm.new_str(source_path), vm)?;
    vm.run_code_obj(code_obj, scope)
}

fn handle_exception<T>(vm: &VirtualMachine, result: PyResult<T>) {
    if let Err(err) = result {
        print_exception(vm, &err);
        process::exit(1);
    }
}

fn run_command(vm: &VirtualMachine, scope: Scope, source: String) -> PyResult<()> {
    debug!("Running command {}", source);
    _run_string(vm, scope, &source, "<stdin>".to_string())?;
    Ok(())
}

fn run_module(vm: &VirtualMachine, module: &str) -> PyResult<()> {
    debug!("Running module {}", module);
    let runpy = vm.import("runpy", &[], 0)?;
    let run_module_as_main = vm.get_attribute(runpy, "_run_module_as_main")?;
    vm.invoke(&run_module_as_main, vec![vm.new_str(module.to_owned())])?;
    Ok(())
}

fn run_script(vm: &VirtualMachine, scope: Scope, script_file: &str) -> PyResult<()> {
    debug!("Running file {}", script_file);
    // Parse an ast from it:
    let file_path = PathBuf::from(script_file);
    let file_path = if file_path.is_file() {
        file_path
    } else if file_path.is_dir() {
        let main_file_path = file_path.join("__main__.py");
        if main_file_path.is_file() {
            main_file_path
        } else {
            error!(
                "can't find '__main__' module in '{}'",
                file_path.to_str().unwrap()
            );
            process::exit(1);
        }
    } else {
        error!(
            "can't open file '{}': No such file or directory",
            file_path.to_str().unwrap()
        );
        process::exit(1);
    };

    let dir = file_path.parent().unwrap().to_str().unwrap().to_string();
    let sys_path = vm.get_attribute(vm.sys_module.clone(), "path").unwrap();
    vm.call_method(&sys_path, "insert", vec![vm.new_int(0), vm.new_str(dir)])?;

    match util::read_file(&file_path) {
        Ok(source) => {
            _run_string(vm, scope, &source, file_path.to_str().unwrap().to_string())?;
        }
        Err(err) => {
            error!(
                "Failed reading file '{}': {:?}",
                file_path.to_str().unwrap(),
                err.kind()
            );
            process::exit(1);
        }
    }
    Ok(())
}

#[test]
fn test_run_script() {
    let vm: VirtualMachine = Default::default();

    // test file run
    let r = run_script(
        &vm,
        vm.new_scope_with_builtins(),
        "tests/snippets/dir_main/__main__.py",
    );
    assert!(r.is_ok());

    // test module run
    let r = run_script(&vm, vm.new_scope_with_builtins(), "tests/snippets/dir_main");
    assert!(r.is_ok());
}

enum ShellExecResult {
    Ok,
    PyErr(PyObjectRef),
    Continue,
}

fn shell_exec(vm: &VirtualMachine, source: &str, scope: Scope) -> ShellExecResult {
    match vm.compile(source, compile::Mode::Single, "<stdin>".to_string()) {
        Ok(code) => {
            match vm.run_code_obj(code, scope.clone()) {
                Ok(value) => {
                    // Save non-None values as "_"
                    if !vm.is_none(&value) {
                        let key = "_";
                        scope.globals.set_item(key, value, vm).unwrap();
                    }
                    ShellExecResult::Ok
                }
                Err(err) => ShellExecResult::PyErr(err),
            }
        }
        Err(CompileError {
            error: CompileErrorType::Parse(ParseErrorType::EOF),
            ..
        }) => ShellExecResult::Continue,
        Err(err) => ShellExecResult::PyErr(vm.new_syntax_error(&err)),
    }
}

fn get_prompt(vm: &VirtualMachine, prompt_name: &str) -> Option<PyStringRef> {
    vm.get_attribute(vm.sys_module.clone(), prompt_name)
        .and_then(|prompt| vm.to_str(&prompt))
        .ok()
}

#[cfg(not(target_os = "redox"))]
fn run_shell(vm: &VirtualMachine, scope: Scope) -> PyResult<()> {
    use rustyline::{error::ReadlineError, Editor};

    println!(
        "Welcome to the magnificent Rust Python {} interpreter \u{1f631} \u{1f596}",
        crate_version!()
    );

    // Read a single line:
    let mut input = String::new();
    let mut repl = Editor::<()>::new();

    // Retrieve a `history_path_str` dependent on the OS
    let repl_history_path = match dirs::config_dir() {
        Some(mut path) => {
            path.push("rustpython");
            path.push("repl_history.txt");
            path
        }
        None => ".repl_history.txt".into(),
    };

    if !repl_history_path.exists() {
        if let Some(parent) = repl_history_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
    }

    if repl.load_history(&repl_history_path).is_err() {
        println!("No previous history.");
    }

    let mut continuing = false;

    loop {
        let prompt = if continuing {
            get_prompt(vm, "ps2")
        } else {
            get_prompt(vm, "ps1")
        };
        let prompt = match prompt {
            Some(ref s) => s.as_str(),
            None => "",
        };
        let result = match repl.readline(prompt) {
            Ok(line) => {
                debug!("You entered {:?}", line);

                repl.add_history_entry(line.trim_end());

                let stop_continuing = line.is_empty();

                if input.is_empty() {
                    input = line;
                } else {
                    input.push_str(&line);
                }
                input.push_str("\n");

                if continuing {
                    if stop_continuing {
                        continuing = false;
                    } else {
                        continue;
                    }
                }

                match shell_exec(vm, &input, scope.clone()) {
                    ShellExecResult::Ok => {
                        input.clear();
                        Ok(())
                    }
                    ShellExecResult::Continue => {
                        continuing = true;
                        Ok(())
                    }
                    ShellExecResult::PyErr(err) => {
                        input.clear();
                        Err(err)
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                continuing = false;
                input.clear();
                let keyboard_interrupt = vm
                    .new_empty_exception(vm.ctx.exceptions.keyboard_interrupt.clone())
                    .unwrap();
                Err(keyboard_interrupt)
            }
            Err(ReadlineError::Eof) => {
                break;
            }
            Err(err) => {
                eprintln!("Readline error: {:?}", err);
                break;
            }
        };

        if let Err(exc) = result {
            print_exception(vm, &exc);
        }
    }
    repl.save_history(&repl_history_path).unwrap();

    Ok(())
}

#[cfg(target_os = "redox")]
fn run_shell(vm: &VirtualMachine, scope: Scope) -> PyResult<()> {
    use std::io::{self, BufRead, Write};

    println!(
        "Welcome to the magnificent Rust Python {} interpreter!",
        crate_version!()
    );

    fn print_prompt(vm: &VirtualMachine) {
        let prompt = get_prompt(vm, "ps1");
        let prompt = match prompt {
            Some(ref s) => s.as_str(),
            None => "",
        };
        print!("{}", prompt);
        io::stdout().lock().flush().expect("flush failed");
    }

    let stdin = io::stdin();

    print_prompt(vm);
    for line in stdin.lock().lines() {
        let mut line = line.expect("line failed");
        line.push_str("\n");
        match shell_exec(vm, &line, scope.clone()) {
            ShellExecResult::Ok => {}
            ShellExecResult::Continue => println!("Unexpected EOF"),
            ShellExecResult::PyErr(exc) => print_exception(vm, &exc),
        }
        print_prompt(vm);
    }

    Ok(())
}

fn main() {
    // The `_os` forms never panic on non-UTF-8 input; lossy conversion keeps the rest usable.
    let argv = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let env = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    std::process::exit(fleet_cli::run(argv, env))
}

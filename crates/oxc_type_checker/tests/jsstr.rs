use oxc_type_checker::compiler::{Program, ProgramOptions};
use std::path::PathBuf;

#[test]
fn jsstr_ambient_module_bodies_keep_their_dependencies() {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").canonicalize().unwrap();
    let source = directory.join("jsstr.d.ts");
    let program = Program::new(ProgramOptions {
        current_directory: directory,
        root_files: vec![source.clone()],
        config: None,
    });
    let file = program.file(program.file_id(&source).unwrap());
    let imports: Vec<_> = file.imports().iter().map(oxc_str::CompactStr::as_str).collect();
    assert_eq!(
        imports,
        [
            "normal-dependency",
            "lead-dependency",
            "other-dependency",
            "trail-dependency",
            "pair-dependency"
        ]
    );
}

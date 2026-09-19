use std::env;
use std::path::Path;

use syntect::dumps::dump_to_uncompressed_file;
use syntect::parsing::SyntaxDefinition;

fn main() {
    println!("cargo:rerun-if-changed=assets/Swift.sublime-syntax");
    // Runtime `SyntaxSet::build` of two-face stalls first Read paint (GB-5513 PTY fold).
    let swift =
        SyntaxDefinition::load_from_str(include_str!("assets/Swift.sublime-syntax"), true, None)
            .expect("parse Swift.sublime-syntax");
    let mut builder = two_face::syntax::extra_newlines().into_builder();
    builder.add(swift);
    let set = builder.build();
    dump_to_uncompressed_file(
        &set,
        Path::new(&env::var("OUT_DIR").unwrap()).join("syntaxes.bin"),
    )
    .expect("dump syntaxes");
}

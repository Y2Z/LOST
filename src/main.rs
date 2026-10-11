/// Imported the first time LOST runs, so there's something to search.
/// Only in the program: the library leaves it out.
const SAMPLE: &[u8] = include_bytes!("../res/data/sample.lost");

fn main() {
    std::process::exit(lost::cli::run(Some(SAMPLE)));
}

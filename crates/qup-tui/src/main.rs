fn main() {
    let banner = ratatui::text::Line::from("qup-tui scaffold");
    let status = qup::hello();

    println!("{banner:?} - {status}");
}

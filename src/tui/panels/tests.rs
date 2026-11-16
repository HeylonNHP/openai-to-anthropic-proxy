use super::*;

#[test]
fn top_and_bottom_align() {
    let p = Panel::new("test", 20);
    assert_eq!(p.width, 22);
    for line in &p.lines {
        assert_eq!(line.width(), p.width as usize);
    }
    assert!(p.lines[0].starts_with(TL));
    assert!(p.lines[0].ends_with(TR));
    assert!(p.lines[1].starts_with(BL));
    assert!(p.lines[1].ends_with(BR));
}

#[test]
fn rows_pad_right_border() {
    let mut p = Panel::new("test", 10);
    p.row("hi");
    p.row("a long string that overflows the panel width");
    for line in &p.lines {
        assert_eq!(line.width(), p.width as usize, "misaligned: {line:?}");
        assert!(
            line.starts_with(V)
                || line.starts_with(TL)
                || line.starts_with(ML)
                || line.starts_with(BL)
        );
        assert!(
            line.ends_with(V) || line.ends_with(TR) || line.ends_with(MR) || line.ends_with(BR)
        );
    }
}

#[test]
fn rules_look_correct() {
    let mut p = Panel::new("test", 10);
    p.rule();
    assert_eq!(p.lines[2].chars().next(), Some('├'));
    assert_eq!(p.lines[2].chars().last(), Some('┤'));
}

#[test]
fn empty_title_still_renders() {
    let p = Panel::new("", 4);
    assert_eq!(p.width, 6);
    for line in &p.lines {
        assert_eq!(line.width(), p.width as usize);
    }
}

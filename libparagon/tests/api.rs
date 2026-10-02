use libparagon::*;

const HELLO: &str = "module t;\n  initial begin $display(\"hello\"); $finish; end\nendmodule\n";

fn no_disk() -> Options {
    Options {
        read_includes_from_disk: false,
        ..Options::default()
    }
}

#[test]
fn simulate_reports_the_first_missing_stage() {
    let r = block_on(simulate(HELLO)).err();
    assert_eq!(
        r,
        Some(Error::NotImplemented {
            stage: Stage::Parse
        })
    );
}

#[test]
fn simulate_reports_preprocessor_errors_with_positions() {
    let r = block_on(simulate_with("\n`ifdef X\n", &no_disk())).err();
    let Some(Error::Diagnostics(d)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(d.len(), 1);
    assert_eq!(
        d[0].to_string(),
        "%Error: input.sv:3:1: `ifdef not terminated at EOF"
    );
}

#[test]
fn preprocess_expands_macros_and_defines() {
    let opts = Options {
        defines: vec![("W".into(), "8".into())],
        ..no_disk()
    };
    let src = "`define M(x) [x-1:0]\nlogic `M(`W) v;\n";
    let p = block_on(preprocess_with(src, &opts)).unwrap();
    assert_eq!(p.text, "logic [8-1:0] v;\n");
    assert!(p.warnings.is_empty());
}

#[test]
fn warnings_are_returned_with_notes() {
    let p = block_on(preprocess("`define D a\n`define D b\n")).unwrap();
    assert_eq!(p.warnings.len(), 1);
    assert_eq!(
        p.warnings[0].to_string(),
        "%Warning-REDEFMACRO: input.sv:2:9: Redefining existing define: 'D', with different value: 'b'\n    \
         input.sv:1:9: ... Location of previous definition, with value: 'a'"
    );
}

#[test]
fn line_markers_option() {
    let opts = Options {
        line_markers: true,
        ..no_disk()
    };
    let p = block_on(preprocess_with("a\n", &opts)).unwrap();
    assert_eq!(p.text, "`line 1 \"input.sv\" 1\na\n");
}

#[test]
fn includes_need_disk_access() {
    let r = block_on(preprocess_with("`include \"nope.vh\"\n", &no_disk()));
    let Err(Error::Diagnostics(d)) = r else {
        panic!("{r:?}")
    };
    assert!(
        d[0].message
            .starts_with("Cannot find include file: 'nope.vh'")
    );
}

#[test]
fn futures_are_send() {
    fn assert_send<T: Send>(_: T) {}
    assert_send(simulate(HELLO));
    assert_send(preprocess(HELLO));
}

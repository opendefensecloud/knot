//! The heading slug rule, checked against the fixture the web client's
//! `headingSlugs.test.ts` reads too, so editor and public page agree.

use knot_markdown::slug::Slugger;

#[derive(serde::Deserialize)]
struct Case {
    titles: Vec<String>,
    slugs: Vec<String>,
}

#[test]
fn slugs_match_shared_fixture() {
    let raw = include_str!("fixtures/heading_slugs.json");
    let cases: Vec<Case> = serde_json::from_str(raw).expect("fixture parses");
    for case in cases {
        let mut slugger = Slugger::default();
        let got: Vec<String> = case.titles.iter().map(|t| slugger.slug(t)).collect();
        assert_eq!(got, case.slugs, "titles {:?}", case.titles);
    }
}

use super::*;

#[test]
fn history_cursor_keeps_timestamp_ties_visibility_and_message_state() {
    let fixture = Fixture::new();
    let ids: Vec<_> = (0..13).map(|_| fixture.send("owner", "visible")).collect();
    let routine = fixture.send_outgoing(&Outgoing {
        mailbox: "owner",
        subject: Some("[review-note] hidden"),
        body: Some("routine"),
        ..Default::default()
    });
    let agent = fixture.send("lane", "agent-only");
    fixture.execute(
        "INSERT INTO message_audiences (message_id, audience) VALUES (?, 'agent')",
        vec![SqlValue::Text(agent.clone())],
    );
    let foreign = fixture.send("foreign", "outside conversation");
    fixture.execute("UPDATE messages SET created_at = 100", vec![]);
    let snapshot = || {
        fixture
            .store
            .readers
            .with(|connection| {
                let mut statement = connection
                    .prepare("SELECT * FROM messages ORDER BY id")
                    .map_err(map_sqlite)?;
                let rows = statement.query_map([], raw_row).map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
            })
            .unwrap()
    };
    let snapshots = snapshot();
    let boxes = ["owner".to_owned(), "lane".to_owned()];
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fixture
            .store
            .conversation_visible_cursor(&boxes, 3, cursor.as_deref())
            .unwrap();
        if page.is_empty() {
            break;
        }
        cursor = Some(page[0]["id"].as_str().unwrap().to_owned());
        seen.extend(
            page.into_iter()
                .rev()
                .map(|row| row["id"].as_str().unwrap().to_owned()),
        );
    }
    assert_eq!(seen, ids.iter().rev().cloned().collect::<Vec<_>>());
    for bad in [
        routine.as_str(),
        agent.as_str(),
        foreign.as_str(),
        "i99999",
        "i01",
    ] {
        assert!(fixture
            .store
            .conversation_visible_cursor(&boxes, 3, Some(bad))
            .is_err());
    }
    assert_eq!(snapshots, snapshot());
}

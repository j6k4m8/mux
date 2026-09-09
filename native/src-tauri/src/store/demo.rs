//! The mailbox a fresh database starts with.
//!
//! Kept apart from the store itself because none of it runs in a real install:
//! it exists so the first launch has something to read, and so paging, MIME
//! parsing, and invitations have fixtures to exercise.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::{now_ms, StoreError};

#[derive(Clone, Copy)]
/// One message in the seeded mailbox. Bodies are written out rather than
/// generated: a demo mailbox is the first thing anyone sees, and templated
/// filler reads like filler.
struct DemoMessage {
    sender: &'static str,
    email: &'static str,
    from_me: bool,
    minutes_ago: i64,
    body_text: &'static str,
    /// Empty when the message arrived as plain text only, which is a case the
    /// reader has to handle.
    body_html: &'static str,
    blocked_remote: i64,
}

struct DemoThread {
    account_id: &'static str,
    subject: &'static str,
    participants: &'static str,
    snippet: &'static str,
    category: &'static str,
    in_inbox: i64,
    unread: i64,
    starred: i64,
    messages: &'static [DemoMessage],
}

/// A remote image a seeded message references but does not load.
struct DemoRemoteImage {
    resource_id: i64,
    url: &'static str,
    domain: &'static str,
    alt_text: &'static str,
}

const DRAFT_THREAD: &[DemoMessage] = &[
    DemoMessage {
        sender: "Tomas Lindqvist",
        email: "tomas@lindqvist.example",
        from_me: false,
        minutes_ago: 1_560,
        body_text: "I took your note about the methods section and rewrote the middle two paragraphs. I think it reads better now.\n\nThe problem is that the figure order stopped making sense once I moved things around — Figure 3 gets referenced before Figure 2 in two places. Do you mind if I just renumber them?",
        body_html: "",
        blocked_remote: 0,
    },
    DemoMessage {
        sender: "Jordan Matelsky",
        email: "jordan@acme.example",
        from_me: true,
        minutes_ago: 1_320,
        body_text: "Renumber, definitely. Nobody has ever loved a paper for its figure numbering.\n\nOne thing on the new middle section: you say the effect holds across both cohorts, but the second cohort is n=14. I would soften that to \"consistent with\" rather than \"holds across\".",
        body_html: "",
        blocked_remote: 0,
    },
    DemoMessage {
        sender: "Priya Raghavan",
        email: "priya@raghavan.example",
        from_me: false,
        minutes_ago: 300,
        body_text: "Agreed on softening it, and I would go a step further — pull the second cohort out of the main claim entirely and put it in the supplement. It is a nice corroboration but it cannot carry weight at that size.\n\nHappy to write that paragraph if you would rather not touch it again.",
        body_html: "",
        blocked_remote: 0,
    },
    DemoMessage {
        sender: "Tomas Lindqvist",
        email: "tomas@lindqvist.example",
        from_me: false,
        minutes_ago: 41,
        body_text: "Let's do that. Priya, if you take the supplement paragraph I'll fix the numbering and push a draft 4 tonight.\n\nAlso, the deadline moved to the 14th, so we have a week more than I thought. No need to rush it.",
        body_html: "",
        blocked_remote: 0,
    },
];

const SHOP_THREAD: &[DemoMessage] = &[DemoMessage {
    sender: "Zoomie Cycle",
    email: "service@zoomiecycle.example",
    from_me: false,
    minutes_ago: 180,
    body_text: "Your wheel is ready\n\nReady for pickup any time this week. We close at 6 on Saturday.\n\nRear wheel rebuild  $85.00\nSpokes (4)  $12.00\nLabour  $40.00\nTotal  $137.00\n\nWorth saying: the hub was in better shape than we expected, so we left it alone rather than charging you for a service you did not need.\n\n— Dana, Zoomie Cycle",
    body_html: r#"<style>.card{background-color: #ffffff; padding: 22px}.h{font-size: 20px; font-weight: 700; color: #14181f}.muted{color: #6b7280; font-size: 12px}.amt{font-weight: 700}</style><table width="100%" cellpadding="0" cellspacing="0" border="0" style="background-color: #f1f3f7"><tbody><tr><td align="center" style="padding: 20px 0"><table class="card" width="470" cellpadding="0" cellspacing="0" border="0"><tbody><tr><td><mux-remote-image data-id="1"></mux-remote-image><p class="h">Your wheel is ready</p><p class="muted">Ready for pickup any time this week. We close at 6 on Saturday.</p><table width="100%" cellpadding="0" cellspacing="0" border="0" style="margin: 16px 0"><tbody><tr><td>Rear wheel rebuild</td><td align="right" class="amt">$85.00</td></tr><tr><td>Spokes (4)</td><td align="right" class="amt">$12.00</td></tr><tr><td>Labour</td><td align="right" class="amt">$40.00</td></tr><tr><td style="padding-top: 8px; border-top: 1px solid #e5e7eb">Total</td><td align="right" class="amt" style="padding-top: 8px; border-top: 1px solid #e5e7eb">$137.00</td></tr></tbody></table><p>Worth saying: the hub was in better shape than we expected, so we left it alone rather than charging you for a service you did not need.</p><p class="muted">— Dana, Zoomie Cycle</p></td></tr></tbody></table></td></tr></tbody></table><mux-remote-image data-id="2"></mux-remote-image>"#,
    blocked_remote: 2,
}];

const LEFTOVERS_THREAD: &[DemoMessage] = &[DemoMessage {
    sender: "Marcus Bell",
    email: "marcus@bell.example",
    from_me: false,
    minutes_ago: 1_560 + 1_440,
    body_text: "You left the good tupperware here. It has your name on it in sharpie, which I respect.\n\nAround Sunday if you want it back.",
    body_html: "",
    blocked_remote: 0,
}];

const RADIATOR_THREAD: &[DemoMessage] = &[DemoMessage {
    sender: "Building management",
    email: "notices@thornhill-management.example",
    from_me: false,
    minutes_ago: 2 * 1_440 + 300,
    body_text: "A technician will service the radiator in your unit on Thursday between 9am and 12pm.\n\nSomeone will need to be home to let them in. If that window does not work, reply to this message and we will try to move you to the afternoon, though Thursday is the only day the contractor is in the building.",
    body_html: "",
    blocked_remote: 0,
}];

/// Not in any mailbox view. This thread exists so the long-thread, MIME, and
/// invitation fixtures have somewhere to live without landing in the inbox the
/// demo is meant to show.
const FIXTURE_THREAD: &[DemoMessage] = &[DemoMessage {
    sender: "Sarah Johnson",
    email: "sarah@studio.example",
    from_me: false,
    minutes_ago: 5 * 1_440,
    body_text: "Development fixture thread. Long-thread paging, MIME parsing, and invitation handling are exercised here.",
    body_html: "",
    blocked_remote: 0,
}];

const DEMO_THREADS: &[DemoThread] = &[
    DemoThread {
        account_id: "acc_work",
        subject: "draft 3 — the methods section reads better now",
        participants: "Tomas Lindqvist, Priya Raghavan",
        snippet:
            "Let's do that. Priya, if you take the supplement paragraph I'll fix the numbering",
        category: "Work",
        in_inbox: 1,
        unread: 1,
        starred: 1,
        messages: DRAFT_THREAD,
    },
    DemoThread {
        account_id: "acc_personal",
        subject: "your wheel is ready",
        participants: "Zoomie Cycle",
        snippet: "Ready for pickup any time this week. We close at 6 on Saturday.",
        category: "Personal",
        in_inbox: 1,
        unread: 1,
        starred: 0,
        messages: SHOP_THREAD,
    },
    DemoThread {
        account_id: "acc_personal",
        subject: "leftovers",
        participants: "Marcus Bell",
        snippet:
            "You left the good tupperware here. It has your name on it in sharpie, which I respect.",
        category: "Personal",
        in_inbox: 1,
        unread: 0,
        starred: 0,
        messages: LEFTOVERS_THREAD,
    },
    DemoThread {
        account_id: "acc_personal",
        subject: "radiator service — Thursday between 9 and 12",
        participants: "Building management",
        snippet:
            "A technician will service the radiator in your unit on Thursday between 9am and 12pm.",
        category: "Personal",
        in_inbox: 1,
        unread: 0,
        starred: 0,
        messages: RADIATOR_THREAD,
    },
    DemoThread {
        account_id: "acc_work",
        subject: "Development fixtures",
        participants: "Sarah Johnson",
        snippet: "Development fixture thread.",
        category: "Work",
        in_inbox: 0,
        unread: 0,
        starred: 0,
        messages: FIXTURE_THREAD,
    },
];

/// The images the shop message references. Nothing fetches them; they exist so
/// the reader has real candidates to withhold and count.
const DEMO_REMOTE_IMAGES: &[(i64, &[DemoRemoteImage])] = &[(
    2,
    &[
        DemoRemoteImage {
            resource_id: 1,
            url: "https://cdn.zoomiecycle.example/logo.png",
            domain: "cdn.zoomiecycle.example",
            alt_text: "Zoomie Cycle",
        },
        DemoRemoteImage {
            resource_id: 2,
            url: "https://track.mailer.example/open?id=8f21c",
            domain: "track.mailer.example",
            alt_text: "",
        },
    ],
)];

/// The thread that carries development fixtures rather than demo content.
pub(crate) const DEMO_FIXTURE_THREAD_ID: i64 = 5;

pub(super) fn seed_demo_mailbox(connection: &mut Connection) -> Result<(), StoreError> {
    let was_already_seeded = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'seed_complete'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if was_already_seeded.as_deref() == Some("1") {
        return Ok(());
    }

    let thread_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
    if thread_count > 0 {
        return Ok(());
    }

    let now = now_ms();
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accounts = [
        (
            "acc_personal",
            "Personal",
            "jordan@example.com",
            "#8b68ff",
            "Jordan",
        ),
        (
            "acc_research",
            "Research",
            "jordan@research.example",
            "#21b89a",
            "Jordan Matelsky\nResearch",
        ),
        (
            "acc_work",
            "Work",
            "jordan@acme.example",
            "#3b82f6",
            "Jordan Matelsky\nMux",
        ),
    ];
    for (id, name, email, color, signature) in accounts {
        transaction.execute(
            "INSERT INTO accounts(id, name, email, color, provider, signature)
             VALUES(?1, ?2, ?3, ?4, 'fake', ?5)",
            params![id, name, email, color, signature],
        )?;
    }

    let mut message_id = 0i64;
    for (index, thread) in DEMO_THREADS.iter().enumerate() {
        let thread_id = index as i64 + 1;
        let latest_at = now
            - thread
                .messages
                .iter()
                .map(|message| message.minutes_ago)
                .min()
                .unwrap_or(0)
                * 60_000;
        let has_from_me = i64::from(thread.messages.iter().any(|message| message.from_me));
        let has_link = i64::from(
            thread
                .messages
                .iter()
                .any(|message| message.body_html.contains("http")),
        );
        transaction.execute(
            "INSERT INTO threads(
               id, account_id, subject, participants, snippet, latest_at, message_count,
               remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
               has_link, has_from_me, category, attachment_names
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, 0, ?11, ?12, ?13, '')",
            params![
                thread_id,
                thread.account_id,
                thread.subject,
                thread.participants,
                thread.snippet,
                latest_at,
                thread.messages.len() as i64,
                thread.in_inbox,
                thread.unread,
                thread.starred,
                has_link,
                has_from_me,
                thread.category,
            ],
        )?;

        let mut searchable = String::new();
        for message in thread.messages {
            message_id += 1;
            transaction.execute(
                "INSERT INTO messages(
                   id, thread_id, sender_name, sender_email, recipients, sent_at,
                   body_text, body_html, blocked_remote_resources, is_from_me
                 ) VALUES(?1, ?2, ?3, ?4, 'Jordan Matelsky <jordan@acme.example>', ?5, ?6, ?7, ?8, ?9)",
                params![
                    message_id,
                    thread_id,
                    message.sender,
                    message.email,
                    now - message.minutes_ago * 60_000,
                    message.body_text,
                    message.body_html,
                    message.blocked_remote,
                    i64::from(message.from_me),
                ],
            )?;
            if !searchable.is_empty() {
                searchable.push('\n');
            }
            searchable.push_str(message.body_text);

            if let Some((_, images)) = DEMO_REMOTE_IMAGES
                .iter()
                .find(|(target, _)| *target == thread_id)
            {
                for image in *images {
                    transaction.execute(
                        "INSERT INTO message_remote_images(
                           message_id, resource_id, url, domain, alt_text
                         ) VALUES(?1, ?2, ?3, ?4, ?5)",
                        params![
                            message_id,
                            image.resource_id,
                            image.url,
                            image.domain,
                            image.alt_text
                        ],
                    )?;
                }
            }
        }

        transaction.execute(
            "INSERT INTO messages_fts(thread_id, subject, participants, body, attachment_names)
             VALUES(?1, ?2, ?3, ?4, '')",
            params![thread_id, thread.subject, thread.participants, searchable],
        )?;
    }

    transaction.execute(
        "INSERT INTO invitations(
           thread_id, uid, title, start_at, end_at, timezone, location,
           organizer, attendees, response, conflict_text
         ) VALUES(?1, 'mux-native-demo-invite', 'Mux architecture review', ?2, ?3,
                  'America/New_York', 'Conference Room A', 'Sarah Johnson',
                  'Jordan Matelsky', 'needsAction', NULL)",
        params![DEMO_FIXTURE_THREAD_ID, now + 86_400_000, now + 90_000_000],
    )?;
    transaction.execute(
        "UPDATE threads SET has_invite = 1 WHERE id = ?1",
        params![DEMO_FIXTURE_THREAD_ID],
    )?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES('seed_complete', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

#[derive(Clone, Copy)]
struct LongDemoThread {
    thread_id: i64,
    account_email: &'static str,
    primary_name: &'static str,
    primary_email: &'static str,
    secondary_name: &'static str,
    secondary_email: &'static str,
    topic: &'static str,
    extra_messages: i64,
}

pub(super) fn seed_demo_account_signatures(connection: &Connection) -> Result<(), StoreError> {
    let is_demo = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'seed_complete'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if is_demo.as_deref() != Some("1") {
        return Ok(());
    }
    for (account_id, signature) in [
        ("acc_personal", "Jordan"),
        ("acc_research", "Jordan Matelsky\nResearch"),
        ("acc_work", "Jordan Matelsky\nMux"),
    ] {
        connection.execute(
            "UPDATE accounts SET signature = ?1 WHERE id = ?2 AND signature = ''",
            params![signature, account_id],
        )?;
    }
    Ok(())
}

pub(super) fn seed_long_demo_threads(connection: &mut Connection) -> Result<(), StoreError> {
    let is_demo = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'seed_complete'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let already_seeded = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'long_demo_threads_v1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if is_demo.as_deref() != Some("1") || already_seeded.as_deref() == Some("1") {
        return Ok(());
    }

    let scenarios = [
        // Paging is exercised on the fixture thread so the curated demo
        // conversations keep the shape a reader would actually receive.
        LongDemoThread {
            thread_id: DEMO_FIXTURE_THREAD_ID,
            account_email: "jordan@acme.example",
            primary_name: "Sarah Johnson",
            primary_email: "sarah@studio.example",
            secondary_name: "Kevin Chang",
            secondary_email: "kevin@studio.example",
            topic: "the architecture review",
            extra_messages: 34,
        },
    ];
    let scenario_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM threads WHERE id = 5", [], |row| {
            row.get(0)
        })?;
    if scenario_count != scenarios.len() as i64 {
        return Ok(());
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for scenario in scenarios {
        let latest_at: i64 = transaction.query_row(
            "SELECT latest_at FROM threads WHERE id = ?1",
            [scenario.thread_id],
            |row| row.get(0),
        )?;
        let participants = format!("{}, {}", scenario.primary_name, scenario.secondary_name);
        let outgoing_recipients = format!(
            "{} <{}>, {} <{}>",
            scenario.primary_name,
            scenario.primary_email,
            scenario.secondary_name,
            scenario.secondary_email
        );
        for index in 0..scenario.extra_messages {
            let is_from_me = index % 3 == 1;
            let secondary_speaking = index % 4 == 2;
            let (sender_name, sender_email) = if is_from_me {
                ("Jordan Matelsky", "jordan@mux.example")
            } else if secondary_speaking {
                (scenario.secondary_name, scenario.secondary_email)
            } else {
                (scenario.primary_name, scenario.primary_email)
            };
            let other_recipient = if sender_email == scenario.primary_email {
                format!("{} <{}>", scenario.secondary_name, scenario.secondary_email)
            } else {
                format!("{} <{}>", scenario.primary_name, scenario.primary_email)
            };
            let recipients = if is_from_me {
                outgoing_recipients.clone()
            } else {
                format!("Jordan <{}>, {}", scenario.account_email, other_recipient)
            };
            let body = long_demo_message(scenario.topic, index, is_from_me);
            let body_html = if index % 9 == 4 {
                format!(
                    "<p><strong>Checkpoint {}</strong> for {}:</p><ul><li>Confirm the owner</li><li>Resolve the open note</li><li>Post the final update</li></ul>",
                    index + 1,
                    scenario.topic
                )
            } else {
                String::new()
            };
            let sent_at = latest_at - (scenario.extra_messages - index + 1) * 7_200_000;
            transaction.execute(
                "INSERT INTO messages(
                   thread_id, sender_name, sender_email, recipients, sent_at, body_text, body_html, is_from_me
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    scenario.thread_id,
                    sender_name,
                    sender_email,
                    recipients,
                    sent_at,
                    body,
                    body_html,
                    i64::from(is_from_me),
                ],
            )?;
        }

        transaction.execute(
            "UPDATE messages
             SET recipients = ?1
             WHERE id = (
               SELECT id FROM messages
               WHERE thread_id = ?2 AND is_from_me = 1
               ORDER BY sent_at DESC, id DESC LIMIT 1
             )",
            params![outgoing_recipients, scenario.thread_id],
        )?;
        transaction.execute(
            "UPDATE threads
             SET participants = ?1,
                 message_count = (SELECT COUNT(*) FROM messages WHERE thread_id = ?2)
             WHERE id = ?2",
            params![participants, scenario.thread_id],
        )?;
        crate::provider_ingest::rebuild_search_index_for_thread(&transaction, scenario.thread_id)?;
    }
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES('long_demo_threads_v1', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

fn long_demo_message(topic: &str, index: i64, is_from_me: bool) -> String {
    let sequence = index + 1;
    let text = match (is_from_me, index % 6) {
        (true, 0) => "I folded those notes into the working draft. The remaining question is small enough to settle in the next pass.",
        (true, 1) => "That works for me. I will keep the current owner and add a concrete decision date so the handoff is unambiguous.",
        (true, 2) => "I checked the latest version against our earlier assumptions. Nothing else needs to move before we share it.",
        (true, 3) => "Good catch. I rewrote that section and left one comment where I still want a second set of eyes.",
        (true, 4) => "I can take the follow-up. I will post a short summary here once the review is complete.",
        (true, _) => "The update is in. I kept the scope narrow and preserved the earlier decision so we do not reopen finished work.",
        (false, 0) => "I reviewed the new draft this morning. The structure is clearer, and I only have one question about the handoff.",
        (false, 1) => "The numbers now line up with the source sheet. I marked the two places where the wording still implies the old plan.",
        (false, 2) => "One small concern: the owner is clear, but the decision date is not. Could we make that explicit before circulation?",
        (false, 3) => "This version reads well. I tested the example against the edge case from yesterday and the outcome is consistent.",
        (false, 4) => "I added comments inline and resolved the ones that were purely editorial. The remaining note needs a product call.",
        (false, _) => "No blocker from me. Once the final wording lands, this is ready to move to the next person.",
    };
    format!("Update {sequence} on {topic}:\n\n{text}")
}

pub(super) fn seed_safe_content_demo(connection: &mut Connection) -> Result<(), StoreError> {
    let is_demo = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'seed_complete'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let already_seeded = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'safe_content_demo_v1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if is_demo.as_deref() != Some("1") || already_seeded.as_deref() == Some("1") {
        return Ok(());
    }

    let raw = concat!(
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/related; boundary=mux-demo\r\n\r\n",
        "--mux-demo\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
        "<p><strong>Latest design review</strong></p>",
        "<p>The annotated diagram is included below. A remote tracking image in the original message was blocked.</p>",
        "<p><a href=\"https://example.com/review\">Open the review notes</a></p>",
        "<script>window.location='https://tracker.invalid'</script>",
        "<img src=\"https://tracker.invalid/pixel\"><img src=\"cid:mux-chart\">\r\n",
        "--mux-demo\r\nContent-Type: image/png; name=\"architecture-preview.png\"\r\n",
        "Content-Disposition: inline; filename=\"architecture-preview.png\"\r\n",
        "Content-ID: <mux-chart>\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\r\n",
        "--mux-demo\r\nContent-Type: text/plain; name=\"review-notes.txt\"\r\n",
        "Content-Disposition: attachment; filename=\"review-notes.txt\"\r\n\r\n",
        "Design review notes\n\n- Keep the interaction model direct.\n- Preserve the local projection.\r\n",
        "--mux-demo--\r\n"
    );
    let parsed = crate::content::parse_mime(raw.as_bytes()).map_err(StoreError::Validation)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let message_id: i64 = transaction.query_row(
        "SELECT id FROM messages WHERE thread_id = ?1 AND is_from_me = 0 ORDER BY sent_at, id LIMIT 1",
        params![DEMO_FIXTURE_THREAD_ID],
        |row| row.get(0),
    )?;
    transaction.execute(
        "UPDATE messages
         SET body_text = ?1, body_html = ?2, blocked_remote_resources = ?3
         WHERE id = ?4",
        params![
            parsed.body_text,
            parsed.body_html,
            parsed.blocked_remote_resources,
            message_id
        ],
    )?;
    let mut attachment_names = Vec::new();
    for (index, attachment) in parsed.attachments.into_iter().enumerate() {
        let attachment_id = format!("demo_safe_{message_id}_{index}");
        let byte_length = attachment.bytes.len() as i64;
        attachment_names.push(attachment.filename.clone());
        transaction.execute(
            "INSERT INTO attachments(
               id, message_id, filename, media_type, byte_length, content, content_id, disposition
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                attachment_id,
                message_id,
                attachment.filename,
                attachment.media_type,
                byte_length,
                attachment.bytes,
                attachment.content_id,
                attachment.disposition
            ],
        )?;
    }
    let attachment_names = attachment_names.join(" ");
    transaction.execute(
        "UPDATE threads
         SET has_attachment = 1, has_link = 1, attachment_names = ?1,
             snippet = 'The annotated design review is ready; one remote image was blocked.'
         WHERE id = 2",
        [&attachment_names],
    )?;
    crate::provider_ingest::rebuild_search_index_for_thread(&transaction, 2)?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES('safe_content_demo_v1', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn seed_demo_threading_headers(connection: &Connection) -> Result<(), StoreError> {
    let is_demo = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'seed_complete'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let already_seeded = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'demo_threading_headers_v1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if is_demo.as_deref() != Some("1") || already_seeded.as_deref() == Some("1") {
        return Ok(());
    }
    connection.execute(
        "UPDATE messages
         SET internet_message_id = '<demo-message-' || id || '@mux.invalid>'
         WHERE internet_message_id = ''",
        [],
    )?;
    connection.execute(
        "INSERT INTO meta(key, value) VALUES('demo_threading_headers_v1', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    Ok(())
}

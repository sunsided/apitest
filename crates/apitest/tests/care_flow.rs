//! A multi-actor, multi-service flow written with the ergonomic layer:
//!
//! 1. Global setup: an existing Hub admin creates a Social Service user
//!    (deleted again when the suite ends).
//! 2. A patient registers on the app as a demo user,
//! 3. confirms their e-mail by opening the link from the mail mock,
//! 4. logs in and creates an activation code.
//! 5. The social worker identifies the patient with that code (opens a case),
//! 6. stores therapy data for the case,
//! 7. and moves the case to "in progress".
//!
//! Everything created along the way is rolled back in reverse order.
//!
//! cargo test -p apitest --features full --test `care_flow`

/// The hub, app and mail services under test.
#[path = "support/care.rs"]
mod care;

/// The onboarding suite: one patient's way from registration to a running case.
mod onboarding {
    use apitest::prelude::*;
    use serde::Deserialize;

    /// Password used for every account the suite creates.
    const PASSWORD: &str = "correct horse battery";

    /// Directory with the pinned hub and app specs.
    fn specs() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/care")
    }

    /// Base suite: both services are pinned to their 1.0 contracts and checked for drift.
    #[apitest::suite]
    fn onboarding() -> Result<Suite> {
        Ok(Suite::new("onboarding")
            .contract(
                Contract::pinned("hub", specs().join("hub-1.0.yaml"))?.live_spec("/openapi.json"),
            )
            .contract(
                Contract::pinned("app", specs().join("app-1.0.yaml"))?.live_spec("/openapi.json"),
            ))
    }

    /// The pre-existing Hub admin: a named actor from the run config
    /// (`actors:` in apitest.nova), logging in lazily with ${`HUB_ADMIN_EMAIL`} / ${`HUB_ADMIN_PASSWORD`}.
    #[apitest::fixture(scope = "suite")]
    #[expect(
        clippy::unused_async,
        reason = "the fixture macro only accepts `async fn`"
    )]
    async fn hub_admin() -> Result<Actor> {
        actor("hub_admin")
    }

    /// Global setup: the admin creates a Social Service user, removed after the suite.
    #[apitest::fixture(scope = "suite")]
    async fn social_worker(hub_admin: Actor) -> Result<Actor> {
        let email = unique_email("social");
        hub_admin
            .call("createUser")
            .json(json!({ "email": email, "password": PASSWORD, "role": "social_service" }))
            .expect(201)
            .rollback(|user| {
                hub_admin
                    .call("deleteUser")
                    .path("id", &user["id"])
                    .expect(204)
            })
            .await?;

        // Logs in lazily on first use; no token handling here.
        Ok(Actor::on("hub").login(email, PASSWORD))
    }

    /// An e-mail as the mail mock returns it; only the body matters here.
    #[derive(Deserialize)]
    struct Mail {
        /// HTML body, containing the confirmation link.
        html: String,
    }

    /// Waits for the confirmation e-mail sent to `email` and opens its link.
    async fn open_confirmation_link(mail: &Actor, email: &str) -> Result<()> {
        let message: Mail = eventually!(
            mail.get("/messages")
                .query("to", email)
                .expect(200)
                .await?
                .get("/0")
        )?;
        visit(&link_containing(&message.html, "/confirm")?).await?;

        Ok(())
    }

    /// Registers `email` as a demo patient and confirms the address, one step each.
    async fn register_confirmed_patient(email: &str) -> Result<()> {
        let (app, mail) = (Actor::on("app"), Actor::on("mail"));
        step(
            "patient registers as a demo user",
            app.call("registerPatient")
                .json(json!({ "email": email, "password": PASSWORD, "demo": true }))
                .expect(201)
                .expect_json(json!({ "status": "pending_confirmation", "demo": true })),
        )
        .await?;
        undo(mail.delete("/messages").query("to", email))?;

        step(
            "patient confirms the e-mail by opening the link",
            open_confirmation_link(&mail, email),
        )
        .await
    }

    /// Logs the confirmed patient in and returns a fresh activation code, one step each.
    async fn activation_code_for(email: &str) -> Result<String> {
        let patient = step(
            "patient logs in",
            Actor::on("app").login(email, PASSWORD).signed_in(),
        )
        .await?;
        undo(patient.call("deletePatient").expect(204))?;

        step(
            "patient creates an activation code",
            patient.call("createActivationCode").expect(201),
        )
        .await?
        .get("/code")
    }

    /// Stores therapy data for the case and moves it to `in_progress`, one step each.
    async fn start_case(social_worker: &Actor, case_id: &str) -> Result<()> {
        step(
            "social worker records therapy data",
            social_worker
                .call("putTherapy")
                .path("id", case_id)
                .json(json!({ "plan": "CBT", "sessions_per_week": 2 }))
                .expect(200),
        )
        .await?;

        step(
            "case moves to in progress",
            social_worker
                .call("updateCase")
                .path("id", case_id)
                .json(json!({ "status": "in_progress" }))
                .expect(200),
        )
        .await?;

        Ok(())
    }

    /// The happy path across all three services ends with a started case that carries
    /// the patient and the therapy data; every created record is rolled back.
    #[apitest::test]
    async fn patient_onboarding(social_worker: Actor) -> Result<()> {
        let email = unique_email("patient");
        register_confirmed_patient(&email).await?;
        let code = activation_code_for(&email).await?;

        let case = step(
            "social worker identifies the patient by code",
            social_worker
                .call("identifyPatient")
                .json(json!({ "activation_code": code }))
                .expect(201)
                .rollback(|case| {
                    social_worker
                        .call("deleteCase")
                        .path("id", &case["id"])
                        .expect(204)
                }),
        )
        .await?;
        let case_id: String = case.get("/id")?;
        start_case(&social_worker, &case_id).await?;

        let current = social_worker
            .call("getCase")
            .path("id", &case_id)
            .expect(200)
            .await?;
        assert_json!(current, {
            "status": "in_progress",
            "patient_id": case["patient_id"],
            "therapy": { "plan": "CBT", "sessions_per_week": 2 }
        });

        Ok(())
    }

    /// The odd case: a case cannot start without therapy data. Same building blocks,
    /// plus an explicit `cx` and a hand-written cleanup to show the escape hatches.
    #[apitest::test]
    async fn case_cannot_start_without_therapy(cx: Ctx, social_worker: Actor) -> Result<()> {
        let email = unique_email("patient");
        Actor::on("app")
            .call("registerPatient")
            .json(json!({ "email": email, "password": PASSWORD, "demo": true }))
            .expect(201)
            .await?;
        let mail = Actor::on("mail");
        undo(mail.delete("/messages").query("to", &email))?;
        open_confirmation_link(&mail, &email).await?;
        let patient = Actor::on("app").login(&email, PASSWORD);
        undo(patient.call("deletePatient").expect(204))?;
        let code: String = patient
            .call("createActivationCode")
            .expect(201)
            .await?
            .get("/code")?;

        let case_id: String = social_worker
            .call("identifyPatient")
            .json(json!({ "activation_code": code }))
            .expect(201)
            .await?
            .get("/id")?;
        // Hand-written cleanup with full control (here: via the explicit context).
        let (sw, id) = (social_worker.clone(), case_id.clone());
        cx.defer("close and delete the case", move |_cx| async move {
            sw.call("deleteCase").path("id", &id).expect(204).await?;
            Ok(())
        });

        let refused = social_worker
            .call("updateCase")
            .path("id", &case_id)
            .json(json!({ "status": "in_progress" }))
            .expect(409)
            .await?;
        assert_json!(refused, { "error": "a case needs therapy data before it can start" });

        Ok(())
    }
}

/// Starts the three services, runs the registered suites against them and finally proves
/// that the rollbacks left no test data behind.
fn main() {
    apitest::harness::main(|| async {
        let services = care::Care::start().await?;
        let hub_url = services.hub.base_url();
        Ok(apitest::harness::Plan::new(services.config())
            .registered()?
            .finally("everything was rolled back", move || async move {
                let left = care::Care::leftovers(&hub_url).await?;
                let clean = serde_json::json!({ "staff": 0, "patients": 0, "activation_codes": 0, "cases": 0, "mails": 0 });
                anyhow::ensure!(left == clean, "leftover test data: {left}");
                Ok(())
            })
            .keep(services))
    })
}

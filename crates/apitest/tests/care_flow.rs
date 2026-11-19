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
//! cargo test -p apitest --features full --test care_flow

#[path = "support/care.rs"]
mod care;

mod onboarding {
    use apitest::prelude::*;
    use serde::Deserialize;

    const PASSWORD: &str = "correct horse battery";

    fn specs() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/care")
    }

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
    /// (`actors:` in apitest.nova), logging in lazily with ${HUB_ADMIN_EMAIL} / ${HUB_ADMIN_PASSWORD}.
    #[apitest::fixture(scope = "suite")]
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

    #[derive(Deserialize)]
    struct Mail {
        html: String,
    }

    #[apitest::test]
    async fn patient_onboarding(social_worker: Actor) -> Result<()> {
        let (app, mail) = (Actor::on("app"), Actor::on("mail"));
        let email = unique_email("patient");

        step(
            "patient registers as a demo user",
            app.call("registerPatient")
                .json(json!({ "email": email, "password": PASSWORD, "demo": true }))
                .expect(201)
                .expect_json(json!({ "status": "pending_confirmation", "demo": true })),
        )
        .await?;
        undo(mail.delete("/messages").query("to", &email))?;

        step("patient confirms the e-mail by opening the link", async {
            let message: Mail = eventually!(
                mail.get("/messages")
                    .query("to", &email)
                    .expect(200)
                    .await?
                    .get("/0")
            )?;
            visit(&link_containing(&message.html, "/confirm")?).await
        })
        .await?;

        let patient = step(
            "patient logs in",
            Actor::on("app").login(&email, PASSWORD).signed_in(),
        )
        .await?;
        undo(patient.call("deletePatient").expect(204))?;

        let code: String = step(
            "patient creates an activation code",
            patient.call("createActivationCode").expect(201),
        )
        .await?
        .get("/code")?;

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

        step(
            "social worker records therapy data",
            social_worker
                .call("putTherapy")
                .path("id", &case_id)
                .json(json!({ "plan": "CBT", "sessions_per_week": 2 }))
                .expect(200),
        )
        .await?;

        step(
            "case moves to in progress",
            social_worker
                .call("updateCase")
                .path("id", &case_id)
                .json(json!({ "status": "in_progress" }))
                .expect(200),
        )
        .await?;

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
        let message: Mail = eventually!(
            mail.get("/messages")
                .query("to", &email)
                .expect(200)
                .await?
                .get("/0")
        )?;
        visit(&link_containing(&message.html, "/confirm")?).await?;
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

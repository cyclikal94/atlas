use crate::*;
use atlas_core::error::ErrorCode;
impl App {
    pub(crate) async fn password_permit(&self) -> Result<PasswordPermit, ApiError> {
        let admission = self
            .password_admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!(ErrorCode::RateLimited))?;
        let work = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.password_slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| anyhow!(ErrorCode::RateLimited))?
        .map_err(|_| anyhow!(ErrorCode::InternalError))?;
        Ok(PasswordPermit {
            _admission: admission,
            _work: work,
        })
    }
    pub async fn new(store: Store) -> Result<Self> {
        let dummy = hash_password(Uuid::new_v4().to_string()).await?;
        Ok(Self {
            store,
            integrations: integrations::IntegrationConfig::from_env()?,
            calendar_slots: Arc::new(Semaphore::new(2)),
            password_slots: Arc::new(Semaphore::new(2)),
            password_admission: Arc::new(Semaphore::new(10)),
            unknown_principal_budget: Arc::new(Mutex::new(throttle::Budget::new(4096, 10))),
            principal_budget: Arc::new(Mutex::new(throttle::Budget::new(4096, 10))),
            source_budget: Arc::new(Mutex::new(throttle::Budget::new(4096, 120))),
            started: Instant::now(),
            trusted_proxies: Arc::new(Vec::new()),
            dummy_hash: Arc::new(dummy),
            directory_enabled: true,
            browser: None,
            oidc: None,
            native_redirects: Arc::new(Vec::new()),
        })
    }
    pub fn trust_proxies(mut self, addresses: Vec<IpAddr>) -> Self {
        self.trusted_proxies = Arc::new(addresses);
        self
    }
    pub fn directory_enabled(mut self, enabled: bool) -> Self {
        self.directory_enabled = enabled;
        self
    }
    pub fn public_origin(mut self, origin: &str) -> Result<Self> {
        self.browser = Some(browser::Config::new(origin)?);
        Ok(self)
    }
    pub fn oidc(
        mut self,
        issuer: &str,
        client_id: &str,
        secret: Option<String>,
        provision: bool,
    ) -> Result<Self> {
        self.oidc = Some(Arc::new(oidc::Config::new(
            self.browser
                .as_ref()
                .ok_or_else(|| anyhow!("OIDC requires ATLAS_PUBLIC_ORIGIN"))?,
            issuer,
            client_id,
            secret,
            provision,
        )?));
        Ok(self)
    }
    pub fn oidc_native_redirects(mut self, redirects: Vec<String>) -> Result<Self> {
        self.native_redirects = Arc::new(
            redirects
                .into_iter()
                .map(|uri| oidc::native_redirect(&uri))
                .collect::<Result<Vec<_>>>()?,
        );
        Ok(self)
    }
    pub fn router(self) -> Router {
        Router::new()
            .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
            .route("/ready", get(ready))
            .route("/api/experimental/v1/me", get(sessions::me))
            .route(
                "/api/experimental/v1/calendar-commands",
                post(calendars::commands),
            )
            .route(
                "/api/experimental/v1/calendar-sources",
                get(calendars::sources),
            )
            .route(
                "/api/experimental/v1/calendar-sources/{id}/import",
                post(calendars::import).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
            )
            .route(
                "/api/experimental/v1/calendar-sources/{id}/refresh",
                post(calendars::refresh),
            )
            .route("/api/experimental/v1/events", get(calendars::events))
            .route("/api/experimental/v1/review-items", get(calendars::reviews))
            .route(
                "/api/experimental/v1/tasks/{id}/anchor",
                get(calendars::anchor),
            )
            .route(
                "/api/experimental/v1/reminder-commands",
                post(calendars::reminder_commands),
            )
            .route("/api/experimental/v1/reminders", get(calendars::reminders))
            .route(
                "/api/experimental/v1/notification-subscriptions",
                get(calendars::subscriptions),
            )
            .route(
                "/api/experimental/v1/notification-capabilities",
                get(calendars::capabilities),
            )
            .route(
                "/api/experimental/v1/notification-deliveries",
                get(calendars::deliveries),
            )
            .route("/api/experimental/v1/task-commands", post(tasks::commands))
            .route(
                "/api/experimental/v1/task-access-commands",
                post(tasks::access),
            )
            .route("/api/experimental/v1/tasks", get(tasks::tasks))
            .route("/api/experimental/v1/task-presets", get(tasks::presets))
            .route("/api/experimental/v1/tasks/{id}", get(tasks::detail))
            .route("/api/experimental/v1/tasks/{id}/rota", get(tasks::rota))
            .route(
                "/api/experimental/v1/tasks/{id}/enrolment",
                get(tasks::enrolment),
            )
            .route("/api/experimental/v1/tasks/{id}/streak", get(tasks::streak))
            .route("/api/experimental/v1/occurrences", get(tasks::occurrences))
            .route(
                "/api/experimental/v1/occurrences/{id}/dependencies",
                get(tasks::dependencies),
            )
            .route("/api/experimental/v1/daily", get(tasks::daily))
            .route(
                "/api/experimental/v1/occurrences/{id}/timers",
                get(tasks::timers),
            )
            .route("/api/experimental/v1/lists", get(tasks::lists))
            .route("/api/experimental/v1/people", get(people::people))
            .route("/api/experimental/v1/people/{id}", get(people::detail))
            .route(
                "/api/experimental/v1/people-commands",
                post(people::command),
            )
            .route(
                "/api/experimental/v1/people/merge-preview",
                post(people::preview),
            )
            .route(
                "/api/experimental/v1/people/requests",
                get(people::requests),
            )
            .route(
                "/api/experimental/v1/people/{id}/duplicates",
                get(people::duplicates),
            )
            .route("/api/experimental/v1/progress", get(tasks::progress))
            .route(
                "/api/experimental/v1/progress/{id}/entries",
                get(tasks::journal),
            )
            .route("/api/experimental/v1/fields", get(tasks::fields))
            .route("/api/experimental/v1/devices", get(sessions::devices))
            .route(
                "/api/experimental/v1/devices/{device_id}",
                delete(sessions::forget_device),
            )
            .route(
                "/api/experimental/v1/oidc/identities",
                delete(sessions::unlink_oidc),
            )
            .route(
                "/api/experimental/v1/sessions",
                post(login).get(sessions::list),
            )
            .route(
                "/api/experimental/v1/sessions/{id}",
                delete(sessions::revoke),
            )
            .route(
                "/api/experimental/v1/password",
                post(sessions::change_password),
            )
            .route(
                "/api/experimental/v1/account-invitations",
                post(onboarding::invite).get(onboarding::list),
            )
            .route(
                "/api/experimental/v1/account-invitations/{id}",
                delete(onboarding::revoke),
            )
            .route(
                "/api/experimental/v1/registration",
                post(onboarding::register),
            )
            .route(
                "/api/experimental/v1/registration/preview",
                post(onboarding::preview),
            )
            .route(
                "/api/experimental/v1/browser-registration",
                post(onboarding::browser_register),
            )
            .route(
                "/api/experimental/v1/oidc/native/start",
                get(oidc::native_start),
            )
            .route(
                "/api/experimental/v1/oidc/native/exchange",
                post(oidc::native_exchange),
            )
            .route("/api/experimental/v1/oidc/start", post(oidc::start))
            .route("/api/experimental/v1/oidc/callback", get(oidc::callback))
            .route(
                "/api/experimental/v1/browser-sessions",
                post(browser::login),
            )
            .route(
                "/api/experimental/v1/browser-sessions/current",
                get(browser::current),
            )
            .route("/api/experimental/v1/sessions/current", delete(logout))
            .route("/api/experimental/v1/commands", post(commands))
            .route(
                "/api/experimental/v1/access-commands",
                post(access_commands),
            )
            .route("/api/experimental/v1/sync", get(sync))
            .route("/api/experimental/v1/households", get(households))
            .route("/api/experimental/v1/invitations", get(invitations))
            .route("/api/experimental/v1/defaults", get(defaults))
            .route(
                "/api/experimental/v1/defaults/templates",
                get(default_templates),
            )
            .route("/api/experimental/v1/policies/{id}", get(resource_policy))
            .route("/api/experimental/v1/directory", get(directory))
            .route("/api/experimental/v1/management-commands", post(management))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .layer(middleware::from_fn(no_store))
            .with_state(self)
    }
}

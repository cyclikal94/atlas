# Remaining release work

The backend implements tasks, shared progress, people, households, offline sync,
calendar enrichment, reminders and authentication. Frontend clients remain separate.
Deployment, recovery and release validation remain to be implemented.

1. Provide Docker Compose and Helm with external PostgreSQL or an optional supplied
   database. Containers will use PostgreSQL; SQLite remains available for direct host
   execution with one process and local persistent storage.
2. Implement versioned export and backup/restore workflows. Restore into a fresh
   installation, protect integration keys, invalidate stale sync cursors and preserve
   operation identities. Define the supported release migration policy at first release.
3. Run two PostgreSQL-backed application replicas to verify worker claims, generation
   checks, retries and initialisation. Measure the global publication gate before
   changing lock or worker transaction boundaries.
4. Validate representative tasks/people/calendar workloads on Linux ARM64 and AMD64,
   and local macOS development. Record current resource limits and operator recovery
   instructions. Prototype measurements do not establish current production capacity.
5. Finish release automation, configuration reference, deployment observability and
   secrets handling. Exercise clean install, restore, actual release upgrades and both
   database/architecture combinations before claiming release readiness.

Native APNs/FCM credentialed transport interoperability and client-local scheduling
need explicit implementation/validation beyond the current Web Push/ntfy adapters.
PWAs cannot promise native OS background scheduling while closed. Future clients must
make reminder delivery ownership clear and reconcile duplicate/offline notifications.

Federation, third-party plugins, calendar editing, email delivery, external task-manager
integration and gift purchase/reservation workflows are not committed backend scope.
Keep boundaries extensible without adding unused frameworks.


# Remaining release work

The backend implements tasks, shared progress, people, households, offline sync,
calendar enrichment, reminders and authentication. Frontend clients remain separate.

M6 supplies PostgreSQL-only Docker, Compose and Helm deployment, versioned same-engine
export/restore, mounted secret configuration, two-replica verification, generated licence
notices and release artefact workflows. SQLite remains available for direct host
execution. Operational procedures are in [operations](operations.md),
[recovery](recovery.md) and [packaging](packaging.md); verified scope and limitations are
in [testing](testing.md).

Before publishing the first supported release:

1. Run the checked-in CI matrix on the release candidate, including native Linux ARM64
   and AMD64. Local AMD64 emulation is useful evidence, not a native-run substitute.
2. Choose supported deployment sizes and run representative sustained workloads on
   those machines. The short validation samples do not establish production capacity.
   Measure the global publication gate before changing transaction boundaries.
3. Freeze the first supported schema/API baseline and publish the migration policy
   described in recovery documentation. There is no prior released version to upgrade
   from today; subsequent releases must test upgrades from supported release versions
   and rollback through backup/restore before publication.
4. Review artefacts from a manual Release workflow run, then push a version tag to
   publish the GitHub Release, container image and Helm chart as described in
   [release packaging](packaging.md). Confirm package visibility and anonymous pulls.
   Workflow configuration does not imply that remote checks have already run.

Native APNs/FCM credentialed transport interoperability and client-local scheduling
need explicit implementation/validation beyond the current Web Push/ntfy adapters.
PWAs cannot promise native OS background scheduling while closed. Future clients must
make reminder delivery ownership clear and reconcile duplicate/offline notifications.

Federation, third-party plugins, calendar editing, email delivery, external task-manager
integration and gift purchase/reservation workflows are not committed backend scope.
Keep boundaries extensible without adding unused frameworks.

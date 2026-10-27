# Image build identity and publication

New image tags use `xoa-image-YYYYMMDD-HHMMSSnnnnnnnnn-<source-sha7>`.
The date and final application source SHA remain in their established positions;
the middle UTC build identifier distinguishes same-day rebuilds. Existing tags
remain readable. API consumers still order publication by `published_at`.

The identity is selected once before Packer starts, reused during Packer retries,
and stamped into `/etc/xoa-hl-image-tag`. A new agent invocation creates a new
identity. The RPM's release tag and the legacy human-readable VM stamp remain
separate from the exact image tag.

Before creating a draft, the publisher creates a provenance commit in
`build-xoa-hl`, parented to the exact builder source SHA. Its
`xoa-hl-image-build.json` records both source SHAs, the image identity and the
selected RPM tag/asset. No branch is moved. The image tag is explicitly reserved
at that commit; an existing tag or release is an error, never an overwrite.
Only after asset upload and final provenance succeeds is the draft published
with `make_latest: "true"`. Upload retries retain this identity.

A failure can leave an unpublished tag or draft; the next image build uses a new
identity. Do not retarget old tags or overwrite a published image to recover.
Build commit time and actual publication time are different; `created_at` and
API list order are still not publication-order contracts.

Before releasing this agent, merge and deploy the companion Jenkins registry
change accepting both legacy and new tags. Deploy XOA's prefix/asset-based
resolver accepts both; the agent and deploy QA retain the final source-SHA
suffix check. The existing #291 normalization PR is the base of this change.

Acceptance needs two image builds from identical source commits on the same UTC
day: distinct tags, distinct provenance commits, correct parents/manifests and
stamped image tags, both discovered by release-watch, and the newer public image
selected by the published-time resolver. No production release is created by PR
fixtures.

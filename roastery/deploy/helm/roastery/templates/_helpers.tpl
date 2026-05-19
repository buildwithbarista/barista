{{/*
Standard chart helpers: name, fullname, chart label, label sets, and
the chart-wide consistency validator (`roastery.validate`).

Every render path includes `roastery.validate` exactly once (it lives
at the bottom of `deployment.yaml`). The validator emits Helm `fail`
errors with actionable messages when the value combination is
internally inconsistent — e.g. `auth.bearer.create: true` with no
`tokens` provided.
*/}}

{{/*
Expand the chart name.
*/}}
{{- define "roastery.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Generate the fully qualified app name.

If `fullnameOverride` is set, use it verbatim. Otherwise prefix the
release name with the chart name unless they already match.
*/}}
{{- define "roastery.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Chart name + version label.
*/}}
{{- define "roastery.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels applied to every resource.
*/}}
{{- define "roastery.labels" -}}
helm.sh/chart: {{ include "roastery.chart" . }}
{{ include "roastery.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: barista
{{- end -}}

{{/*
Selector labels — the stable subset used by Services and label
selectors. Never include `version` here (changing the version label
would break rolling updates).
*/}}
{{- define "roastery.selectorLabels" -}}
app.kubernetes.io/name: {{ include "roastery.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Service-account name (created or pre-existing).
*/}}
{{- define "roastery.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "roastery.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Resolved image reference: repository + (override tag | Chart.AppVersion).
*/}}
{{- define "roastery.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/*
Map the user-facing storage backend name (`filesystem`/`s3`/`gcs`) to
the binary's `ROASTERY_STORAGE_BACKEND` value (`fs`/`s3`/`gcs`).
*/}}
{{- define "roastery.storageBackendEnv" -}}
{{- $backend := .Values.storage.backend -}}
{{- if eq $backend "filesystem" -}}fs{{- else -}}{{- $backend -}}{{- end -}}
{{- end -}}

{{/*
Secret name for the bearer-tokens file.
*/}}
{{- define "roastery.bearerSecretName" -}}
{{- if .Values.auth.bearer.create -}}
{{- printf "%s-bearer-tokens" (include "roastery.fullname" .) -}}
{{- else -}}
{{- .Values.auth.bearer.existingSecret -}}
{{- end -}}
{{- end -}}

{{/*
Secret name for the mTLS CA cert.
*/}}
{{- define "roastery.mtlsSecretName" -}}
{{- if .Values.auth.mtls.create -}}
{{- printf "%s-mtls-ca" (include "roastery.fullname" .) -}}
{{- else -}}
{{- .Values.auth.mtls.existingSecret -}}
{{- end -}}
{{- end -}}

{{/*
Secret name for the server TLS cert + key.
*/}}
{{- define "roastery.tlsSecretName" -}}
{{- if .Values.tls.create -}}
{{- printf "%s-tls" (include "roastery.fullname" .) -}}
{{- else -}}
{{- .Values.tls.existingSecret -}}
{{- end -}}
{{- end -}}

{{/*
PVC name (filesystem backend with persistence enabled).
*/}}
{{- define "roastery.pvcName" -}}
{{- printf "%s-data" (include "roastery.fullname" .) -}}
{{- end -}}

{{/*
Whether a chart-managed PVC will be rendered + mounted. The
filesystem backend is the only one that uses local disk; for s3/gcs
the in-pod storage dir holds nothing of interest and an emptyDir
suffices.
*/}}
{{- define "roastery.useChartPvc" -}}
{{- if and .Values.persistence.enabled (eq .Values.storage.backend "filesystem") -}}true{{- end -}}
{{- end -}}

{{/*
Chart-wide consistency check. Emits `fail` with an actionable
message when the value combination cannot be deployed safely.

The checks here cover the rules `ServerConfig::validate` enforces in
the binary, plus a few chart-level ones (multi-replica + filesystem,
chart-managed secret without payload) that the binary can't see.

Reference: roastery/src/config.rs::ServerConfig::validate (BAR-AUTH-*,
BAR-CACHE-*) and roastery/README.md "Authentication" /
"Upstream-on-miss".
*/}}
{{- define "roastery.validate" -}}

{{/* (1) Multi-replica + filesystem PVC is RWO-incompatible. */}}
{{- if and (gt (int .Values.replicaCount) 1) (eq .Values.storage.backend "filesystem") -}}
{{- fail "roastery: replicaCount > 1 with storage.backend=filesystem is unsupported (the PVC accessMode is ReadWriteOnce). Use storage.backend=s3 or storage.backend=gcs for multi-replica deployments." -}}
{{- end -}}

{{/* (2) s3 backend requires bucket + region. */}}
{{- if eq .Values.storage.backend "s3" -}}
{{- if not .Values.storage.s3.bucket -}}
{{- fail "roastery: storage.backend=s3 requires storage.s3.bucket." -}}
{{- end -}}
{{- if not .Values.storage.s3.region -}}
{{- fail "roastery: storage.backend=s3 requires storage.s3.region." -}}
{{- end -}}
{{- end -}}

{{/* (3) gcs backend requires bucket + project. */}}
{{- if eq .Values.storage.backend "gcs" -}}
{{- if not .Values.storage.gcs.bucket -}}
{{- fail "roastery: storage.backend=gcs requires storage.gcs.bucket." -}}
{{- end -}}
{{- if not .Values.storage.gcs.project -}}
{{- fail "roastery: storage.backend=gcs requires storage.gcs.project." -}}
{{- end -}}
{{- end -}}

{{/* (4) TLS: pick exactly one of create / existingSecret when enabled. */}}
{{- if .Values.tls.enabled -}}
{{- if and .Values.tls.create .Values.tls.existingSecret -}}
{{- fail "roastery: tls.create and tls.existingSecret are mutually exclusive — set one, not both." -}}
{{- end -}}
{{- if not (or .Values.tls.create .Values.tls.existingSecret) -}}
{{- fail "roastery: tls.enabled=true requires either tls.create=true (with tls.certPem + tls.keyPem) or tls.existingSecret=<name>." -}}
{{- end -}}
{{- if .Values.tls.create -}}
{{- if not .Values.tls.certPem -}}
{{- fail "roastery: tls.create=true requires tls.certPem (use --set-file tls.certPem=cert.pem)." -}}
{{- end -}}
{{- if not .Values.tls.keyPem -}}
{{- fail "roastery: tls.create=true requires tls.keyPem (use --set-file tls.keyPem=key.pem)." -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* (5) Bearer auth: pick exactly one of create / existingSecret when enabled. */}}
{{- if .Values.auth.bearer.enabled -}}
{{- if and .Values.auth.bearer.create .Values.auth.bearer.existingSecret -}}
{{- fail "roastery: auth.bearer.create and auth.bearer.existingSecret are mutually exclusive — set one, not both." -}}
{{- end -}}
{{- if not (or .Values.auth.bearer.create .Values.auth.bearer.existingSecret) -}}
{{- fail "roastery: auth.bearer.enabled=true requires either auth.bearer.create=true (with auth.bearer.tokens) or auth.bearer.existingSecret=<name>." -}}
{{- end -}}
{{- if and .Values.auth.bearer.create (not .Values.auth.bearer.tokens) -}}
{{- fail "roastery: auth.bearer.create=true requires auth.bearer.tokens (use --set-file auth.bearer.tokens=tokens.txt)." -}}
{{- end -}}
{{- end -}}

{{/* (6) mTLS auth: pick exactly one of create / existingSecret when enabled. */}}
{{- if .Values.auth.mtls.enabled -}}
{{- if and .Values.auth.mtls.create .Values.auth.mtls.existingSecret -}}
{{- fail "roastery: auth.mtls.create and auth.mtls.existingSecret are mutually exclusive — set one, not both." -}}
{{- end -}}
{{- if not (or .Values.auth.mtls.create .Values.auth.mtls.existingSecret) -}}
{{- fail "roastery: auth.mtls.enabled=true requires either auth.mtls.create=true (with auth.mtls.caCertPem) or auth.mtls.existingSecret=<name>." -}}
{{- end -}}
{{- if and .Values.auth.mtls.create (not .Values.auth.mtls.caCertPem) -}}
{{- fail "roastery: auth.mtls.create=true requires auth.mtls.caCertPem (use --set-file auth.mtls.caCertPem=ca.pem)." -}}
{{- end -}}
{{/* (7) mTLS requires server TLS — mirrors the binary's BAR-AUTH validation. */}}
{{- if not .Values.tls.enabled -}}
{{- fail "roastery: auth.mtls.enabled=true requires tls.enabled=true (the binary refuses to start mTLS without server-side TLS — see roastery/README.md 'mTLS')." -}}
{{- end -}}
{{- end -}}

{{/* (8) Non-loopback bind without any auth is the BAR-AUTH-005 fail-closed case. */}}
{{- $bindHost := regexReplaceAll ":[0-9]+$" .Values.server.bind "" -}}
{{- $isLoopback := or (eq $bindHost "127.0.0.1") (or (eq $bindHost "::1") (eq $bindHost "localhost")) -}}
{{- if and (not $isLoopback) (not (or .Values.auth.bearer.enabled .Values.auth.mtls.enabled)) -}}
{{- fail (printf "roastery: server.bind=%q is non-loopback and no auth mechanism is enabled — the binary will refuse to start (BAR-AUTH-005). Enable auth.bearer or auth.mtls, or bind to 127.0.0.1." .Values.server.bind) -}}
{{- end -}}

{{/* (9) upstream.fetchMissing requires at least one repo (binary's BAR-CACHE-007). */}}
{{- if .Values.upstream.fetchMissing -}}
{{- if eq (len .Values.upstream.repos) 0 -}}
{{- fail "roastery: upstream.fetchMissing=true requires at least one entry in upstream.repos (the binary surfaces this as BAR-CACHE-007 at startup)." -}}
{{- end -}}
{{- end -}}

{{/* (10) PDB sanity. */}}
{{- if and .Values.podDisruptionBudget.enabled (le (int .Values.replicaCount) 1) -}}
{{- fail "roastery: podDisruptionBudget.enabled=true requires replicaCount > 1." -}}
{{- end -}}

{{- end -}}

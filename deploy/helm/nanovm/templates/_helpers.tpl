{{/*
Expand the name of the chart.
*/}}
{{- define "nanovm.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name. Truncated at 63 chars because some Kubernetes
name fields are limited to that.
*/}}
{{- define "nanovm.fullname" -}}
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
Common labels applied to every object.
*/}}
{{- define "nanovm.labels" -}}
app.kubernetes.io/name: {{ include "nanovm.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{/*
Selector labels.
*/}}
{{- define "nanovm.selectorLabels" -}}
app.kubernetes.io/name: {{ include "nanovm.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Name of the Secret holding NANOVM_API_TOKENS — either the one this
chart creates from `.Values.config.apiTokens` or an existing one
referenced via `.Values.tokensSecret.existingSecret`. Operators
managing tokens out-of-band (sealed-secrets / ExternalSecrets / a
secret operator) set `existingSecret` to point at their own Secret;
the chart then stops creating its own `*-tokens` and the Deployment
mounts theirs.
*/}}
{{- define "nanovm.secretName" -}}
{{- if .Values.tokensSecret.existingSecret -}}
{{- .Values.tokensSecret.existingSecret -}}
{{- else -}}
{{- include "nanovm.fullname" . -}}-tokens
{{- end -}}
{{- end -}}

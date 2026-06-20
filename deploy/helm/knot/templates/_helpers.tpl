{{/*
Common labels & selectors.
*/}}
{{- define "knot.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "knot.fullname" -}}
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

{{- define "knot.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "knot.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "knot.selectorLabels" -}}
app.kubernetes.io/name: {{ include "knot.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "knot.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "knot.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Secret name/key selectors. Each value (database URL, session key) is resolved
independently: an external Secret + custom key when existingSecretName is set,
otherwise the chart-managed Secret with the canonical KNOT_* key. This is what
makes mixed configs (one external, one inline) work — the old single
knot.secretName only ever looked at database.existingSecretName and silently
dropped the chart Secret (incl. the session key) for partial configs.
*/}}
{{- define "knot.dbSecretName" -}}
{{- .Values.database.existingSecretName | default (include "knot.fullname" .) -}}
{{- end -}}
{{- define "knot.dbSecretKey" -}}
{{- if .Values.database.existingSecretName -}}{{ .Values.database.existingSecretKey }}{{- else -}}KNOT_DATABASE_URL{{- end -}}
{{- end -}}
{{- define "knot.sessionSecretName" -}}
{{- .Values.session.existingSecretName | default (include "knot.fullname" .) -}}
{{- end -}}
{{- define "knot.sessionSecretKey" -}}
{{- if .Values.session.existingSecretName -}}{{ .Values.session.existingSecretKey }}{{- else -}}KNOT_SESSION_KEY{{- end -}}
{{- end -}}

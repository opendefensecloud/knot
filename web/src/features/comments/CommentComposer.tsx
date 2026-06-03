import type React from "react";
import { useState } from "react";
import { useMentionPicker } from "./MentionPicker";

interface Props {
  placeholder?: string;
  submitLabel?: string;
  isPending?: boolean;
  onSubmit: (body: string) => void;
  "data-testid-input"?: string;
  "data-testid-submit"?: string;
}

export function CommentComposer({
  placeholder = "Write a comment…",
  submitLabel = "Submit",
  isPending = false,
  onSubmit,
  "data-testid-input": testidInput,
  "data-testid-submit": testidSubmit,
}: Props) {
  const [body, setBody] = useState("");
  const { textareaProps, picker } = useMentionPicker(body, setBody);

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const trimmed = body.trim();
    if (!trimmed) return;
    onSubmit(trimmed);
    setBody("");
  }

  return (
    <form onSubmit={handleSubmit} style={{ padding: 12 }}>
      <div style={{ position: "relative" }}>
        <textarea
          data-testid={testidInput}
          value={body}
          onChange={(e) => setBody(e.target.value)}
          placeholder={placeholder}
          rows={3}
          style={{ width: "100%", boxSizing: "border-box", resize: "vertical", padding: 8, fontSize: 14 }}
          {...textareaProps}
        />
        {picker}
      </div>
      <button
        type="submit"
        data-testid={testidSubmit}
        disabled={isPending || body.trim().length === 0}
        style={{ marginTop: 8, padding: "6px 14px", cursor: isPending ? "not-allowed" : "pointer" }}
      >
        {isPending ? "Submitting…" : submitLabel}
      </button>
    </form>
  );
}

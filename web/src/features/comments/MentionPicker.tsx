/**
 * MentionPicker — detects "@" in a textarea and shows a member dropdown.
 *
 * Usage: wrap the textarea with <div style={{position:"relative"}}> and
 * spread the returned `textareaProps` onto the <textarea>. The returned
 * `picker` element should be rendered inside that same div.
 */

import type React from "react";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { workspaceApi } from "../workspace/workspace.api";

interface MentionState {
  query: string;
  atOffset: number;
}

function parseMention(value: string, cursorPos: number): MentionState | null {
  let i = cursorPos - 1;
  while (i >= 0 && value[i] !== "@" && !/\s/.test(value[i] ?? "")) {
    i--;
  }
  if (i < 0 || value[i] !== "@") return null;
  const query = value.slice(i + 1, cursorPos);
  // Don't activate if the query contains spaces (already typed past a word boundary)
  if (/\s/.test(query)) return null;
  return { query, atOffset: i };
}

export function useMentionPicker(value: string, onChange: (v: string) => void) {
  const [cursor, setCursor] = useState(0);
  const [highlightIndex, setHighlightIndex] = useState(0);

  const members = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
    staleTime: 60_000,
  });

  const memberList = members.data && "ok" in members.data ? members.data.ok : [];
  const mention = parseMention(value, cursor);

  const filtered = mention
    ? memberList.filter((m) =>
        m.display_name.toLowerCase().includes(mention.query.toLowerCase()) ||
        m.email.toLowerCase().includes(mention.query.toLowerCase()),
      )
    : [];

  function pick(displayName: string) {
    if (!mention) return;
    const before = value.slice(0, mention.atOffset);
    const after = value.slice(cursor);
    onChange(before + `@${displayName} ` + after);
    setHighlightIndex(0);
  }

  const isOpen = mention !== null && filtered.length > 0;

  const textareaProps = {
    onSelect: (e: React.SyntheticEvent<HTMLTextAreaElement>) => {
      setCursor(e.currentTarget.selectionStart);
    },
    onKeyUp: (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      setCursor(e.currentTarget.selectionStart);
    },
    onKeyDown: (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (!isOpen) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setHighlightIndex((i) => Math.min(i + 1, filtered.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setHighlightIndex((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter") {
        const picked = filtered[highlightIndex];
        if (picked) {
          e.preventDefault();
          pick(picked.display_name);
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        // Clear cursor pos so mention state resets
        setCursor(0);
      }
    },
  };

  const picker = isOpen ? (
    <ul
      style={{
        position: "absolute",
        top: "100%",
        left: 0,
        right: 0,
        zIndex: 50,
        background: "white",
        border: "1px solid #ddd",
        borderRadius: 4,
        margin: 0,
        padding: 0,
        listStyle: "none",
        boxShadow: "0 4px 12px rgba(0,0,0,0.1)",
        maxHeight: 200,
        overflowY: "auto",
      }}
      role="listbox"
    >
      {filtered.map((m, i) => (
        <li
          key={m.user_id}
          role="option"
          aria-selected={i === highlightIndex}
          onMouseDown={(e) => {
            e.preventDefault();
            pick(m.display_name);
          }}
          style={{
            padding: "8px 12px",
            cursor: "pointer",
            background: i === highlightIndex ? "#e5e5ff" : "transparent",
            fontSize: 14,
          }}
        >
          <strong>{m.display_name}</strong>{" "}
          <span style={{ color: "#888", fontSize: 12 }}>{m.email}</span>
        </li>
      ))}
    </ul>
  ) : null;

  return { textareaProps, picker };
}

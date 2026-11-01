import type React from "react";
import { autoUpdate, computePosition, flip, offset, size } from "@floating-ui/dom";
import { useEffect, useId, useMemo, useRef, useState } from "react";

import type { Member } from "../../lib/validators";
import { memberMatches } from "../workspace/memberSearch";

/** Tallest the list grows before it scrolls, in px. */
const LIST_MAX_HEIGHT = 240;

/** Members by name, then email, so a long list reads like a directory. */
function sortMembers(members: Member[]): Member[] {
  return [...members].sort(
    (left, right) =>
      left.display_name.localeCompare(right.display_name, undefined, { sensitivity: "base" }) ||
      left.email.localeCompare(right.email, undefined, { sensitivity: "base" }),
  );
}

function memberLabel(member: Member): string {
  return `${member.display_name} (${member.email})`;
}

/**
 * A searchable member dropdown: type to filter by name or email, then pick
 * with the mouse or arrow keys and Enter. `value` is the picked user id, or
 * "" while nothing is picked.
 */
export function MemberPicker({
  members,
  loading = false,
  failed = false,
  value,
  onChange,
  className,
  testId,
}: {
  members: Member[];
  /** The member list is still on its way. */
  loading?: boolean;
  /** The member list couldn't be fetched. */
  failed?: boolean;
  value: string;
  onChange: (userId: string) => void;
  className?: string;
  testId?: string;
}) {
  const listboxId = useId();
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const [highlightIndex, setHighlightIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const popupRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  // Where the pointer last moved; see onMouseMove on the options.
  const lastPointerRef = useRef<{ x: number; y: number } | null>(null);

  const sortedMembers = useMemo(() => sortMembers(members), [members]);
  const selectedMember = members.find((member) => member.user_id === value);
  const matchingMembers = sortedMembers.filter((member) => memberMatches(member, query));
  const activeIndex = Math.max(0, Math.min(highlightIndex, matchingMembers.length - 1));
  const listShown = open && matchingMembers.length > 0;

  // The dialog around the picker scrolls and clips, so the popup is
  // positioned fixed against the input, flipping above it when there's no
  // room below.
  useEffect(() => {
    const input = inputRef.current;
    const popup = popupRef.current;
    if (!open || !input || !popup) return;
    return autoUpdate(input, popup, () => {
      void computePosition(input, popup, {
        strategy: "fixed",
        placement: "bottom-start",
        middleware: [
          offset(4),
          flip({ padding: 8 }),
          size({
            padding: 8,
            apply({ availableHeight, rects, elements }) {
              Object.assign(elements.floating.style, {
                width: `${rects.reference.width}px`,
                maxHeight: `${Math.min(LIST_MAX_HEIGHT, availableHeight)}px`,
              });
            },
          }),
        ],
      }).then(({ x, y }) => {
        Object.assign(popup.style, { left: `${x}px`, top: `${y}px`, visibility: "visible" });
      });
    });
  }, [open]);

  // Keep the highlighted option visible while arrowing through a long list.
  useEffect(() => {
    if (!listShown) return;
    listRef.current?.children[activeIndex]?.scrollIntoView?.({ block: "nearest" });
  }, [listShown, activeIndex]);

  const pick = (member: Member) => {
    onChange(member.user_id);
    setQuery("");
    setOpen(false);
  };

  /** Drop the current pick and search for `text` instead. */
  const search = (text: string) => {
    if (value) onChange("");
    setQuery(text);
    setHighlightIndex(0);
    setOpen(true);
  };

  const moveHighlight = (step: number) => {
    if (!open) {
      setOpen(true);
      return;
    }
    setHighlightIndex(Math.max(0, Math.min(activeIndex + step, matchingMembers.length - 1)));
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      moveHighlight(e.key === "ArrowDown" ? 1 : -1);
    } else if (e.key === "Enter") {
      const highlighted = matchingMembers[activeIndex];
      if (listShown && highlighted) {
        e.preventDefault();
        pick(highlighted);
      }
    } else if (e.key === "Escape" && open) {
      // Close the list without closing the dialog around it.
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    } else if (selectedMember && !e.ctrlKey && !e.metaKey && !e.altKey) {
      // The input shows the pick's label; editing that label would search
      // for it. Treat the first keystroke as the start of a new search.
      if (e.key === "Backspace" || e.key === "Delete") {
        e.preventDefault();
        search("");
      } else if (e.key.length === 1) {
        e.preventDefault();
        search(e.key);
      }
    }
  };

  let statusText: string | null = null;
  if (open && matchingMembers.length === 0) {
    if (loading) statusText = "Loading members…";
    else if (failed) statusText = "Couldn't load members";
    else statusText = "No matching members";
  }

  return (
    <div className={className}>
      <input
        ref={inputRef}
        type="text"
        role="combobox"
        data-testid={testId}
        aria-label="Member"
        aria-autocomplete="list"
        aria-expanded={listShown}
        aria-controls={listShown ? listboxId : undefined}
        aria-activedescendant={listShown ? `${listboxId}-${activeIndex}` : undefined}
        placeholder="Search members…"
        autoComplete="off"
        value={selectedMember ? memberLabel(selectedMember) : query}
        onChange={(e) => search(selectedMember ? "" : e.target.value)}
        onPaste={(e) => {
          if (!selectedMember) return;
          e.preventDefault();
          search(e.clipboardData.getData("text"));
        }}
        onFocus={(e) => {
          e.currentTarget.select();
          setOpen(true);
        }}
        onClick={() => setOpen(true)}
        onBlur={() => {
          setOpen(false);
          // Text that was never picked would read as a choice that isn't one.
          if (!value) setQuery("");
        }}
        onKeyDown={onKeyDown}
        className="w-full h-9 px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm"
      />
      {open && (
        <div
          ref={popupRef}
          // `invisible` until placed, so it doesn't flash in the corner.
          className="invisible fixed top-0 left-0 z-50 bg-surface border border-border rounded-md shadow-lg overflow-y-auto"
        >
          {listShown && (
            <ul ref={listRef} id={listboxId} role="listbox" className="m-0 p-0 list-none">
              {matchingMembers.map((member, index) => (
                <li
                  key={member.user_id}
                  id={`${listboxId}-${index}`}
                  role="option"
                  data-testid={testId ? `${testId}-option-${member.user_id}` : undefined}
                  aria-selected={index === activeIndex}
                  // mousedown, not click: the input's blur would close the list first.
                  onMouseDown={(e) => {
                    e.preventDefault();
                    pick(member);
                  }}
                  // Only a pointer that really moved takes the highlight: when
                  // arrowing scrolls the list under a resting pointer, the
                  // browser can report the newly hovered row too.
                  onMouseMove={(e) => {
                    const last = lastPointerRef.current;
                    if (last && last.x === e.clientX && last.y === e.clientY) return;
                    lastPointerRef.current = { x: e.clientX, y: e.clientY };
                    setHighlightIndex(index);
                  }}
                  className={`px-3 py-2 cursor-pointer text-sm ${
                    index === activeIndex ? "bg-muted text-fg" : "text-fg"
                  }`}
                >
                  <span className="font-medium">{member.display_name}</span>{" "}
                  <span className="text-fg-muted text-[12px]">{member.email}</span>
                </li>
              ))}
            </ul>
          )}
          {statusText && (
            <p role="status" className="m-0 px-3 py-2 text-fg-muted text-[13px]">
              {statusText}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

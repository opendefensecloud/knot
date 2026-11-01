import type { Member } from "../../lib/validators";

/** Case-insensitive substring match on name or email; a blank query matches everyone. */
export function memberMatches(member: Member, query: string): boolean {
  const needle = query.trim().toLowerCase();
  return (
    needle === "" ||
    member.display_name.toLowerCase().includes(needle) ||
    member.email.toLowerCase().includes(needle)
  );
}

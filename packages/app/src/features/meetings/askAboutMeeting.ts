import type { BorgClient } from "@borg/client";

/**
 * The first turn of an "Ask anything" chat about a meeting.
 *
 * The question stays first and the meeting is named after it, in the open:
 * the agent's read-only `meetings` tool fetches the transcript by id, and the
 * user can see in the conversation what Borg was pointed at.
 */
export function meetingQuestion(question: string, meeting: { id: string; title: string }): string {
  return `${question.trim()}\n\n(About the meeting "${meeting.title}", meeting id ${meeting.id}.)`;
}

/**
 * Opens a fresh conversation and sends the question. Resolves with the
 * session id once the daemon has taken the turn (its first event), so the
 * caller can navigate there; rejects with why it did not.
 */
export async function askAboutMeeting(
  borg: BorgClient,
  question: string,
  meeting: { id: string; title: string },
): Promise<string> {
  const { sessionId } = await borg.session.open({});
  await new Promise<void>((resolve, reject) => {
    borg.session.send(
      { sessionId, text: meetingQuestion(question, meeting), images: [] },
      { onMessage: () => resolve(), onError: reject, onComplete: () => resolve() },
    );
  });
  return sessionId;
}

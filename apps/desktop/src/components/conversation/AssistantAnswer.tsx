import MarkdownBody from "../MarkdownBody";

export default function AssistantAnswer(props: { markdown: string; streaming?: boolean; stage?: boolean }) {
  return <div class={`assistant-answer${props.stage ? " assistant-stage-answer" : ""}`} data-part="assistant-answer" data-stage={props.stage ? "true" : undefined}>
    <MarkdownBody class="md-body assistant-answer-markdown" content={props.markdown} final={!props.streaming} />
  </div>;
}

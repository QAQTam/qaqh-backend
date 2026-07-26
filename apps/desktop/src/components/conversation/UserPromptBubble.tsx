export default function UserPromptBubble(props: { text: string }) {
  return <div class="user-prompt-row" data-part="user-prompt">
    <div class="user-prompt-bubble">{props.text}</div>
  </div>;
}

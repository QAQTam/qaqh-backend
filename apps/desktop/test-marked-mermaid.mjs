import { marked, Renderer } from 'marked';

const r = new Renderer();
r.code = ({ text, lang }) => {
  console.log('=== renderer.code called ===');
  console.log('lang:', JSON.stringify(lang));
  console.log('text (JSON):', JSON.stringify(text));
  console.log('text (raw):');
  console.log(text);
  console.log('=== end ===');
  return '<div>test</div>';
};

const markdown = [
  '```mermaid',
  'graph TD',
  '    A <==>|\"WebSocket\"| B',
  '    C -->|\"HTTP\"| D',
  '    E <==>|\"WebSocket<br/>ws://127.0.0.1\"| F',
  '```',
].join('\n');

console.log('--- Input markdown ---');
console.log(markdown);
console.log('--- Parsing ---');
marked.parse(markdown, { renderer: r });

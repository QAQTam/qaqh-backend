const { marked, Renderer } = require('marked');

const r = new Renderer();
r.code = ({ text, lang }) => {
  console.log('=== renderer.code called ===');
  console.log('lang:', JSON.stringify(lang));
  console.log('text:', JSON.stringify(text));
  console.log('text raw:');
  console.log(text);
  console.log('=== end ===');
  return '<div>test</div>';
};

const markdown = [
  '```mermaid',
  'graph TD',
  '    A <==>|\"WebSocket\"| B',
  '    C -->|\"HTTP\"| D',
  '```',
].join('\n');

console.log('--- Input markdown ---');
console.log(markdown);
console.log('--- Parsing ---');
marked.parse(markdown, { renderer: r });

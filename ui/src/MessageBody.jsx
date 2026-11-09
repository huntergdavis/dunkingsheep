import React from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeRaw from 'rehype-raw';
import rehypeSanitize, { defaultSchema } from 'rehype-sanitize';

// Owner-thread text can contain markup from other processes. Interpret only presentation
// elements; never allow active content, remote images, event handlers, styles or forms.
const safeMessageSchema = {
  ...defaultSchema,
  tagNames: ['a', 'blockquote', 'br', 'code', 'del', 'div', 'em', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'hr', 'kbd', 'li', 'ol', 'p', 'pre', 's', 'span', 'strong', 'sub', 'sup', 'table', 'tbody', 'td', 'th', 'thead', 'tr', 'ul'],
  attributes: { a: ['href', 'title'], code: [['className', /^language-[\w-]+$/]] },
  strip: ['script', 'style', 'iframe', 'object', 'embed', 'svg', 'math', 'form'],
};

function plainText(node) {
  if (node.type === 'text') return node.value || '';
  return (node.children || []).map(plainText).join('');
}

function Paragraph({ node, children }) {
  const text = plainText(node);
  const lines = text.split('\n');
  const diagram = lines.length > 1 && lines.some(line => /^\s*[+|│┌└├┤┬┴┼─┐┘]/u.test(line))
    && lines.some(line => /[+|│┌└├┤┬┴┼─┐┘]/u.test(line));
  return <p className={diagram ? 'ascii-diagram' : undefined}>{children}</p>;
}

export default function MessageBody({ body, frontDesk = false }) {
  if (!frontDesk) return <div className="markdown"><ReactMarkdown>{body}</ReactMarkdown></div>;
  return <div className="markdown front-desk-markdown">
    <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeRaw, [rehypeSanitize, safeMessageSchema]]}
      components={{ p: Paragraph, table: ({ children }) => <div className="message-table-scroll"><table>{children}</table></div> }}>
      {body}
    </ReactMarkdown>
  </div>;
}

/**
 * Keeps the single line breaks a reply was written with.
 *
 * Markdown folds a lone newline into a space, which is right for prose wrapped
 * at 80 columns and wrong for what a model actually sends: an address, a short
 * list of names, a signature. Those arrive one per line and would otherwise
 * run together into a single sentence.
 *
 * Only `text` nodes are touched. Fenced and inline code carry their content as
 * a `value` on their own node types, so their newlines never pass through here.
 */

/** The slice of the syntax tree this plugin reads; structurally an mdast node. */
interface TreeNode {
  type: string;
  value?: string;
  children?: TreeNode[];
}

/** Splits one text node on its newlines, with a break between the pieces. */
function withBreaks(node: TreeNode): TreeNode[] {
  const value = node.value ?? "";
  if (!/[\r\n]/.test(value)) return [node];
  const out: TreeNode[] = [];
  // Every line ending a reply can carry: Unix, Windows, and old Mac.
  value.split(/\r?\n|\r/).forEach((line, index) => {
    if (index > 0) out.push({ type: "break" });
    if (line !== "") out.push({ type: "text", value: line });
  });
  return out;
}

function visit(node: TreeNode): void {
  if (!node.children) return;
  node.children = node.children.flatMap((child) => {
    if (child.type === "text") return withBreaks(child);
    visit(child);
    return [child];
  });
}

export function remarkLineBreaks() {
  return (tree: TreeNode): void => visit(tree);
}

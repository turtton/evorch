// Minimal DOM helpers. Text always goes through textContent or text nodes;
// nothing here accepts HTML.

/**
 * Create an element.
 *
 * `props` keys: `class`, `text`, `on` ({event: handler}), `attrs`
 * ({name: value}; null or false omits it), `dataset`, and any element
 * property such as `href`, `type`, `disabled`, `hidden` or `value`.
 */
export function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined) continue;
    switch (key) {
      case "class":
        el.className = value;
        break;
      case "text":
        el.textContent = value;
        break;
      case "on":
        for (const [event, handler] of Object.entries(value)) el.addEventListener(event, handler);
        break;
      case "attrs":
        for (const [name, v] of Object.entries(value)) {
          if (v === null || v === false || v === undefined) continue;
          el.setAttribute(name, v === true ? "" : String(v));
        }
        break;
      case "dataset":
        Object.assign(el.dataset, value);
        break;
      default:
        el[key] = value;
    }
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

/** Replace all children of `el`. */
export function replaceChildren(el, ...children) {
  el.replaceChildren();
  append(el, children);
  return el;
}

/**
 * Keep `container`'s children in the order of `items`, reusing the element
 * made for each key. `create(item)` makes a new element, `update(el, item)`
 * refreshes a reused one. Reuse keeps focus, text selection and typed input.
 */
export function reconcile(container, items, key, create, update) {
  const existing = new Map();
  for (const child of [...container.children]) {
    if (child.dataset.key !== undefined) existing.set(child.dataset.key, child);
  }
  let cursor = container.firstElementChild;
  // Elements without a key (headings, placeholders) stay in front.
  while (cursor && cursor.dataset.key === undefined) cursor = cursor.nextElementSibling;
  for (const item of items) {
    const k = key(item);
    let el = existing.get(k);
    if (el) {
      existing.delete(k);
      update?.(el, item);
    } else {
      el = create(item);
      el.dataset.key = k;
    }
    if (el !== cursor) container.insertBefore(el, cursor);
    else cursor = cursor.nextElementSibling;
  }
  for (const el of existing.values()) el.remove();
}

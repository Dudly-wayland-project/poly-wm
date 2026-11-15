// Reference one-window-per-column scrolling layout.
//
// Copy this file to your configuration path (or load it with
// `poly --config docs/examples/scrolling.js`) to try it. Columns follow the
// compositor-owned tiled order; this layout owns only per-workspace
// presentation state: each column's width and the horizontal viewport offset.
// `arrange` scrolls the viewport just enough to reveal the focused column.
//
// The state is bucketed by `ctx.workspace.id`, because `ctx.state` belongs to
// the layout, not to one workspace. The same shape extends to multi-window
// columns later: keep a list of the window ids in each column, and use the
// `order` effect for consume/expel.

const MIN_COLUMN_WIDTH = 160;
const DEFAULT_WIDTH_RATIO = 0.5;
const RESIZE_STEP = 64;

function defaultWidth(area) {
  return Math.max(MIN_COLUMN_WIDTH, Math.floor(area.w * DEFAULT_WIDTH_RATIO));
}

function stateFor(ctx) {
  const all = ctx.state.workspaces || (ctx.state.workspaces = {});
  let state = all[ctx.workspace.id];
  if (!state) {
    state = { offset: 0, widths: {}, maximized: {} };
    all[ctx.workspace.id] = state;
  }
  state.maximized = state.maximized || {};
  return state;
}

// Maximize (niri's `maximize-column`): the column fills the working area but
// stays in the scrolling layout, so gaps and borders remain. The stored width
// is untouched, so unmaximizing restores the exact previous width.
function isMaximized(state, id) {
  return state.maximized[id] === true;
}

// Columns in content coordinates, before the viewport offset. Returns the
// columns and the total content width.
function measure(ctx, state) {
  const area = ctx.output.work_area;
  const columns = [];
  let total = 0;
  for (const window of ctx.windows) {
    let width = state.widths[window.id] ?? defaultWidth(area);
    width = Math.max(MIN_COLUMN_WIDTH, Math.min(width, area.w));
    state.widths[window.id] = width;
    // Maximizing only changes the presented width; `state.widths` keeps the
    // width to restore. Height already spans the working area.
    const shown = isMaximized(state, window.id) ? area.w : width;
    columns.push({ id: window.id, left: total, width: shown });
    total += shown + ctx.gap;
  }
  if (columns.length > 0) {
    total -= ctx.gap;
  }
  return { columns, total };
}

function columnIndex(columns, id) {
  return columns.findIndex((column) => column.id === id);
}

// Keep only the widths of windows that are still on this workspace.
function prune(state, ctx) {
  const present = new Set(ctx.windows.map((window) => window.id));
  for (const id of Object.keys(state.widths)) {
    if (!present.has(Number(id))) {
      delete state.widths[id];
    }
  }
  for (const id of Object.keys(state.maximized)) {
    if (!present.has(Number(id))) {
      delete state.maximized[id];
    }
  }
}

function clamp(value, low, high) {
  return Math.max(low, Math.min(value, high));
}

// Clamp the viewport and reveal the focused column if it scrolled away.
function viewport(ctx, state, columns, total) {
  const area = ctx.output.work_area;
  const maxOffset = Math.max(0, total - area.w);
  let offset = clamp(state.offset, 0, maxOffset);

  const index = columnIndex(columns, ctx.focused);
  if (index >= 0) {
    const column = columns[index];
    const right = column.left + column.width;
    if (column.left < offset) {
      offset = column.left;
    } else if (right > offset + area.w) {
      offset = right - area.w;
    }
    offset = clamp(offset, 0, maxOffset);
  }

  state.offset = offset;
  return offset;
}

function moveColumn(ctx, state, delta) {
  const { columns } = measure(ctx, state);
  const index = columnIndex(columns, ctx.focused);
  const target = index >= 0 ? index + delta : -1;
  if (target < 0 || target >= columns.length) {
    return null;
  }
  const order = ctx.windows.map((window) => window.id);
  [order[index], order[target]] = [order[target], order[index]];
  return { order: order, relayout: true };
}

function resizeColumn(ctx, state, delta) {
  let { columns } = measure(ctx, state);
  const index = columnIndex(columns, ctx.focused);
  if (index < 0) {
    return null;
  }
  // Like niri: changing the width cancels maximization and resizes from the
  // stored width.
  if (isMaximized(state, ctx.focused)) {
    delete state.maximized[ctx.focused];
    columns = measure(ctx, state).columns;
  }
  const width = columns[index].width + delta;
  state.widths[ctx.focused] = clamp(width, MIN_COLUMN_WIDTH, ctx.output.work_area.w);
  return { relayout: true };
}

// Toggle `maximize-column` for the focused column.
function toggleMaximize(ctx, state) {
  const { columns } = measure(ctx, state);
  const index = columnIndex(columns, ctx.focused);
  if (index < 0) {
    return null;
  }
  if (isMaximized(state, ctx.focused)) {
    delete state.maximized[ctx.focused];
  } else {
    state.maximized[ctx.focused] = true;
  }
  return { relayout: true };
}

function centerColumn(ctx, state) {
  const { columns } = measure(ctx, state);
  const index = columnIndex(columns, ctx.focused);
  if (index < 0) {
    return null;
  }
  const column = columns[index];
  state.offset = column.left + column.width / 2 - ctx.output.work_area.w / 2;
  return { relayout: true };
}

function focusAt(ctx, index) {
  const window = ctx.windows[index];
  if (!window || window.id === ctx.focused) {
    return null;
  }
  return { focus: window.id, relayout: true };
}

config({
  general: {
    mod_key: "Alt",
    gap: 8,
  },
  input: { keyboard: { layout: "us" } },
});

bind("Alt+Q", "close-window");
bind("Alt+Return", { spawn: ["foot"] });

layout("scrolling", {
  arrange(ctx) {
    const state = stateFor(ctx);
    prune(state, ctx);
    const { columns, total } = measure(ctx, state);
    const offset = viewport(ctx, state, columns, total);
    const area = ctx.output.work_area;
    const result = {};
    for (const column of columns) {
      result[column.id] = {
        x: Math.floor(area.x + column.left - offset + 0.5),
        y: area.y,
        w: column.width,
        h: area.h,
      };
    }
    return result;
  },

  actions: {
    "focus-left"(ctx) {
      const state = stateFor(ctx);
      const { columns } = measure(ctx, state);
      const index = columnIndex(columns, ctx.focused);
      if (index < 0) {
        return focusAt(ctx, 0);
      }
      return focusAt(ctx, index - 1);
    },

    "focus-right"(ctx) {
      const state = stateFor(ctx);
      const { columns } = measure(ctx, state);
      const index = columnIndex(columns, ctx.focused);
      if (index < 0) {
        return focusAt(ctx, 0);
      }
      return focusAt(ctx, index + 1);
    },

    "focus-first"(ctx) {
      return focusAt(ctx, 0);
    },

    "focus-last"(ctx) {
      return focusAt(ctx, ctx.windows.length - 1);
    },

    "move-column-left"(ctx) {
      return moveColumn(ctx, stateFor(ctx), -1);
    },

    "move-column-right"(ctx) {
      return moveColumn(ctx, stateFor(ctx), 1);
    },

    "resize-column"(ctx, args) {
      const delta = args[0] === undefined ? RESIZE_STEP : Number(args[0]);
      return resizeColumn(ctx, stateFor(ctx), delta);
    },

    "center-column"(ctx) {
      return centerColumn(ctx, stateFor(ctx));
    },

    "maximize-column"(ctx) {
      return toggleMaximize(ctx, stateFor(ctx));
    },
  },

  keybinds: {
    "Alt+H": "focus-left",
    "Alt+L": "focus-right",
    "Alt+Home": "focus-first",
    "Alt+End": "focus-last",
    "Alt+Shift+H": { action: "move-column-left", key_repeat: false },
    "Alt+Shift+L": { action: "move-column-right", key_repeat: false },
    "Alt+Minus": { action: "resize-column", args: ["-64"], key_repeat: true },
    "Alt+Equal": { action: "resize-column", args: ["64"], key_repeat: true },
    "Alt+W": "center-column",
    "Alt+M": { action: "maximize-column", key_repeat: false },
  },

  // New windows open to the right of the focused column, niri style.
  insert(ctx, window) {
    const state = stateFor(ctx);
    state.widths[window.id] = defaultWidth(ctx.output.work_area);
    if (ctx.focused) {
      const index = ctx.windows.findIndex((existing) => existing.id === ctx.focused);
      if (index >= 0) {
        return index + 2;
      }
    }
    return ctx.windows.length + 1;
  },
});

default_layout("scrolling");

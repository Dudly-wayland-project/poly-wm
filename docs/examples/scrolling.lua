-- Reference one-window-per-column scrolling layout.
--
-- Copy this file to your configuration path (or load it with
-- `poly --config docs/examples/scrolling.lua`) to try it. Columns follow the
-- compositor-owned tiled order; this layout owns only per-workspace
-- presentation state: each column's width and the horizontal viewport offset.
-- `arrange` scrolls the viewport just enough to reveal the focused column.
--
-- The state is bucketed by `ctx.workspace.id`, because `poly.state` belongs to
-- the layout, not to one workspace. The same shape extends to multi-window
-- columns later: keep a list of the window ids in each column, and use the
-- `order` effect for consume/expel.

local MIN_COLUMN_WIDTH = 160
local DEFAULT_WIDTH_RATIO = 0.5
local RESIZE_STEP = 64

local function default_width(area)
  return math.max(MIN_COLUMN_WIDTH, math.floor(area.w * DEFAULT_WIDTH_RATIO))
end

local function state_for(workspace)
  local all = poly.state.workspaces
  if not all then
    all = {}
    poly.state.workspaces = all
  end
  local state = all[workspace.id]
  if not state then
    state = { offset = 0, widths = {} }
    all[workspace.id] = state
  end
  return state
end

-- Columns in content coordinates, before the viewport offset. Returns the
-- columns and the total content width.
local function measure(ctx, state)
  local area = ctx.output.work_area
  local columns = {}
  local total = 0
  for _, window in ipairs(ctx.windows) do
    local width = state.widths[window.id] or default_width(area)
    width = math.max(MIN_COLUMN_WIDTH, math.min(width, area.w))
    state.widths[window.id] = width
    columns[#columns + 1] = { id = window.id, left = total, width = width }
    total = total + width + poly.gap
  end
  if #columns > 0 then
    total = total - poly.gap
  end
  return columns, total
end

local function column_index(columns, id)
  for index, column in ipairs(columns) do
    if column.id == id then
      return index
    end
  end
end

-- Keep only the widths of windows that are still on this workspace.
local function prune(state, ctx)
  local present = {}
  for _, window in ipairs(ctx.windows) do
    present[window.id] = true
  end
  for id in pairs(state.widths) do
    if not present[id] then
      state.widths[id] = nil
    end
  end
end

local function clamp(value, low, high)
  return math.max(low, math.min(value, high))
end

-- Clamp the viewport and reveal the focused column if it scrolled away.
local function viewport(ctx, state, columns, total)
  local area = ctx.output.work_area
  local max_offset = math.max(0, total - area.w)
  local offset = clamp(state.offset, 0, max_offset)

  local index = column_index(columns, ctx.focused)
  if index then
    local column = columns[index]
    local right = column.left + column.width
    if column.left < offset then
      offset = column.left
    elseif right > offset + area.w then
      offset = right - area.w
    end
    offset = clamp(offset, 0, max_offset)
  end

  state.offset = offset
  return offset
end

local function move_column(ctx, state, delta)
  local columns = measure(ctx, state)
  local index = column_index(columns, ctx.focused)
  local target = index and index + delta
  if not target or target < 1 or target > #columns then
    return nil
  end
  local order = {}
  for i, window in ipairs(ctx.windows) do
    order[i] = window.id
  end
  order[index], order[target] = order[target], order[index]
  return { order = order, relayout = true }
end

local function resize_column(ctx, state, delta)
  local columns = measure(ctx, state)
  local index = column_index(columns, ctx.focused)
  if not index then
    return nil
  end
  local width = columns[index].width + delta
  state.widths[ctx.focused] = clamp(width, MIN_COLUMN_WIDTH, ctx.output.work_area.w)
  return { relayout = true }
end

local function center_column(ctx, state)
  local columns = measure(ctx, state)
  local index = column_index(columns, ctx.focused)
  if not index then
    return nil
  end
  local column = columns[index]
  state.offset = column.left + column.width / 2 - ctx.output.work_area.w / 2
  return { relayout = true }
end

local function focus_at(ctx, index)
  local window = ctx.windows[index]
  if not window or window.id == ctx.focused then
    return nil
  end
  return { focus = window.id, relayout = true }
end

return {
  general = {
    mod_key = "Alt",
    gap = 8,
  },
  input = { keyboard = { layout = "us" } },
  keybinds = {
    ["Alt+Q"] = "close-window",
    ["Alt+Return"] = { spawn = { "foot" } },
  },
  layouts = {
    scrolling = {
      arrange = function(ctx)
        local state = state_for(ctx.workspace)
        prune(state, ctx)
        local columns, total = measure(ctx, state)
        local offset = viewport(ctx, state, columns, total)
        local area = ctx.output.work_area
        local result = {}
        for _, column in ipairs(columns) do
          result[column.id] = {
            x = math.floor(area.x + column.left - offset + 0.5),
            y = area.y,
            w = column.width,
            h = area.h,
          }
        end
        return result
      end,

      actions = {
        ["focus-left"] = function(ctx)
          local state = state_for(ctx.workspace)
          local columns = measure(ctx, state)
          local index = column_index(columns, ctx.focused)
          if not index then
            return focus_at(ctx, 1)
          end
          return focus_at(ctx, index - 1)
        end,

        ["focus-right"] = function(ctx)
          local state = state_for(ctx.workspace)
          local columns = measure(ctx, state)
          local index = column_index(columns, ctx.focused)
          if not index then
            return focus_at(ctx, 1)
          end
          return focus_at(ctx, index + 1)
        end,

        ["focus-first"] = function(ctx)
          return focus_at(ctx, 1)
        end,

        ["focus-last"] = function(ctx)
          return focus_at(ctx, #ctx.windows)
        end,

        ["move-column-left"] = function(ctx)
          return move_column(ctx, state_for(ctx.workspace), -1)
        end,

        ["move-column-right"] = function(ctx)
          return move_column(ctx, state_for(ctx.workspace), 1)
        end,

        ["resize-column"] = function(ctx, args)
          local delta = tonumber(args[1]) or RESIZE_STEP
          return resize_column(ctx, state_for(ctx.workspace), delta)
        end,

        ["center-column"] = function(ctx)
          return center_column(ctx, state_for(ctx.workspace))
        end,
      },

      keybinds = {
        ["Alt+H"] = "focus-left",
        ["Alt+L"] = "focus-right",
        ["Alt+Home"] = "focus-first",
        ["Alt+End"] = "focus-last",
        ["Alt+Shift+H"] = { "move-column-left", key_repeat = false },
        ["Alt+Shift+L"] = { "move-column-right", key_repeat = false },
        ["Alt+Minus"] = { action = "resize-column", args = { "-64" }, key_repeat = true },
        ["Alt+Equal"] = { action = "resize-column", args = { "64" }, key_repeat = true },
        ["Alt+W"] = "center-column",
      },

      -- New windows open to the right of the focused column, niri style.
      insert = function(ctx, window)
        local state = state_for(ctx.workspace)
        state.widths[window.id] = default_width(ctx.output.work_area)
        if ctx.focused then
          for index, existing in ipairs(ctx.windows) do
            if existing.id == ctx.focused then
              return index + 1
            end
          end
        end
        return #ctx.windows + 1
      end,
    },
  },
  default_layout = "scrolling",
}

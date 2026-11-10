local h = require("changes_helpers")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq
local has = th.has

local function text(row)
  local parts = {}
  for _, span in ipairs(row) do
    parts[#parts + 1] = span[1]
  end
  return table.concat(parts)
end

local function status_of(raw)
  local files = h.parse_status(raw)
  local paths, codes = {}, {}
  for index, file in ipairs(files) do
    paths[index] = file.path
    codes[index] = file.code
  end
  return paths, codes
end

case("status_records_split_on_nul", function()
  local paths, codes = status_of(" M src/a.rs\0?? notes.md\0")
  eq(#paths, 2)
  eq(paths[1], "src/a.rs")
  eq(codes[1], " M")
  eq(paths[2], "notes.md")
  eq(codes[2], "??")
end)

case("status_keeps_paths_with_spaces", function()
  local paths = status_of("A  dir/my notes.txt\0")
  eq(paths[1], "dir/my notes.txt")
end)

case("status_skips_the_original_path_of_a_rename", function()
  local paths, codes = status_of("R  new/name.rs\0old/name.rs\0 M kept.rs\0")
  eq(#paths, 2)
  eq(paths[1], "new/name.rs")
  eq(codes[1], "R ")
  eq(paths[2], "kept.rs")
end)

case("status_ignores_records_without_a_path", function()
  eq(#status_of("\0\0"), 0)
end)

case("kind_prefers_the_worktree_letter", function()
  local marker, style = h.kind("MM")
  eq(marker, "M")
  eq(style, "warning")
  marker, style = h.kind(" M")
  eq(marker, "M")
  marker, style = h.kind("A ")
  eq(marker, "A")
  eq(style, "success")
  marker, style = h.kind(" D")
  eq(marker, "D")
  eq(style, "error")
  marker, style = h.kind("??")
  eq(marker, "?")
  eq(style, "dim")
end)

case("diff_lines_carry_the_change_style", function()
  local rows = h.parse_diff(
    "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;\n let b = 3;\n"
  )
  eq(#rows, 8)
  eq(rows[1].style, "dim")
  eq(rows[5].style, "tool_path")
  eq(rows[6].style, "diff_old")
  eq(rows[7].style, "diff_new")
  eq(rows[8].style, nil)
end)

case("diff_does_not_mistake_file_headers_for_changes", function()
  local rows = h.parse_diff("--- a/a.rs\n+++ b/a.rs\n")
  eq(rows[1].style, "dim")
  eq(rows[2].style, "dim")
end)

case("diff_stops_at_the_row_budget", function()
  local text = {}
  for i = 1, 10 do
    text[i] = "+line " .. i .. "\n"
  end
  local rows = h.parse_diff(table.concat(text), 4)
  eq(#rows, 5)
  has(rows[5].text, "truncated")
  eq(rows[5].style, "dim")
end)

case("untracked_rows_are_all_additions", function()
  local rows = h.untracked_rows("one\ntwo\n", 100)
  eq(#rows, 2)
  eq(rows[1].text, "one")
  eq(rows[1].style, "diff_new")
  eq(rows[2].style, "diff_new")
end)

case("long_lines_are_truncated_to_the_panel", function()
  local cut = h.truncate("abcdefghij", 5)
  eq(#cut, 5)
  has(cut, "\u{2026}")
end)

case("changed_label_counts", function()
  eq(h.changed_label(0), "clean")
  eq(h.changed_label(1), "1 change")
  eq(h.changed_label(3), "3 changes")
end)

case("render_puts_the_list_above_the_diff", function()
  local state = {
    branch = "main",
    files = { { path = "a.rs", code = " M" }, { path = "b.rs", code = "??" } },
    selected = 2,
    diff = { { text = "+new", style = "diff_new" } },
  }
  local lines, list_first, list_last = h.render(state, 40)
  eq(list_first, 3)
  eq(list_last, 4)
  eq(text(lines[1]), "main \u{b7} 2 changes")
  eq(text(lines[3]), " M a.rs")
  eq(text(lines[4]), " ? b.rs")
  eq(lines[4][1][2], "item_selected")
  eq(lines[3][1][2], "warning")
  eq(text(lines[5]), "")
  eq(text(lines[6]), "+new")
  eq(lines[6][1][2], "diff_new")
  eq(#lines, 6)
end)

case("render_says_clean_without_files", function()
  local lines, list_first, list_last = h.render({ files = {}, selected = 1, diff = {} }, 40)
  eq(text(lines[1]), "clean")
  eq(list_first, 3)
  eq(list_last, 3)
  has(text(lines[3]), "clean")
  eq(#lines, 3)
end)

case("render_shows_the_error_instead_of_a_summary", function()
  local lines = h.render({ error = "not a git repository", files = {}, diff = {} }, 40)
  eq(text(lines[1]), "not a git repository")
end)

th.report()

# S1 content fence for captured hook payloads (M4 W2-D1).
# Keeps keys and nesting; replaces every leaf with a type marker. Arrays keep at most
# three reduced elements plus a "<+N more>" marker. Works with jq 1.6 (bookworm) and 1.8.
def red:
  if type == "string" then "<string>"
  elif type == "number" then "<number>"
  elif type == "boolean" then "<bool>"
  elif type == "null" then "<null>"
  elif type == "array" then
    (if length <= 3 then map(red)
     else (.[0:3] | map(red)) + ["<+\(length - 3) more>"] end)
  elif type == "object" then with_entries(.value |= red)
  else "<unknown>" end;
red

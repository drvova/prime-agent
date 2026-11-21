#!/bin/sh
# A stdio MCP server for the Scheme kernel's MCP tests: newline-delimited
# JSON-RPC on stdin and stdout, in plain POSIX shell so the tests need no
# interpreter the product does not. It matches the client's compact frames
# textually ({"jsonrpc":"2.0","id":N,"method":...}); it is not a JSON parser.
#
# usage: stdio_fixture.sh LOG [crash|mute]
#   LOG    receives one line per message handled, in order.
#   crash  complain on stderr and exit before reading anything.
#   mute   log the process id, then read every message and answer none.

log=$1
if [ "$2" = crash ]; then
  printf 'warming up\n\033[31mfixture: cannot open %s\033[0m\n' "$log" >&2
  exit 3
fi
if [ "$2" = mute ]; then
  echo "$$" >>"$log"
  while IFS= read -r line; do :; done
  exit 0
fi

result() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$1"; }
failure() { printf '{"jsonrpc":"2.0","id":%s,"error":{"code":%s,"message":"%s"}}\n' "$id" "$1" "$2"; }
text() { result "{\"content\":[{\"type\":\"text\",\"text\":\"$1\"}],\"isError\":${2:-false}}"; }

while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      echo initialize >>"$log"
      result '{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"resources":{},"prompts":{}},"serverInfo":{"name":"fixture","version":"1"}}'
      ;;
    *'"method":"notifications/cancelled"'*)
      echo "cancelled $(printf '%s\n' "$line" | sed -n 's/.*"requestId":\([0-9]*\).*/\1/p')" >>"$log"
      ;;
    *'"method":"notifications/initialized"'*)
      echo initialized >>"$log"
      ;;
    *'"method":"tools/list"'*)
      echo tools/list >>"$log"
      case "$line" in
        *'"cursor":"more"'*)
          result '{"tools":[{"name":"fail"},{"name":"hang"},{"name":"shape"},{"name":"ask"},{"name":"secret"}]}'
          ;;
        *)
          result '{"tools":[{"name":"echo","description":"Echo a message","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},{"name":"pid","description":"The server process id"}],"nextCursor":"more"}'
          ;;
      esac
      ;;
    *'"method":"tools/call"'*)
      tool=$(printf '%s\n' "$line" | sed -n 's/.*"params":{"name":"\([a-z]*\)".*/\1/p')
      echo "call $tool" >>"$log"
      case "$tool" in
        echo) text "$(printf '%s\n' "$line" | sed -n 's/.*"message":"\([^"]*\)".*/\1/p')" ;;
        pid) text "$$" ;;
        fail) text 'the tool refused' true ;;
        hang) ;;
        shape) result '{"content":[{"type":"text","text":"two rows"}],"structuredContent":{"rows":2,"tags":["a","b"]}}' ;;
        ask)
          # A request of the client: its answer is the next line.
          printf '{"jsonrpc":"2.0","id":900,"method":"ping"}\n'
          IFS= read -r answer
          case "$answer" in
            '{"jsonrpc":"2.0","id":900,"result":{}}') text answered ;;
            *) text ignored ;;
          esac
          ;;
        *) failure -32602 'Unknown tool' ;;
      esac
      ;;
    *'"method":"resources/list"'*)
      echo resources/list >>"$log"
      result '{"resources":[{"uri":"memo://one","name":"one"}]}'
      ;;
    *'"method":"resources/templates/list"'*)
      echo resources/templates/list >>"$log"
      result '{"resourceTemplates":[{"uriTemplate":"memo://{name}","name":"memo"}]}'
      ;;
    *'"method":"resources/read"'*)
      echo resources/read >>"$log"
      case "$line" in
        *'"uri":"memo://one"'*) result '{"contents":[{"uri":"memo://one","text":"the first memo"}]}' ;;
        *) failure -32002 'Resource not found' ;;
      esac
      ;;
    *'"method":"prompts/list"'*)
      echo prompts/list >>"$log"
      result '{"prompts":[{"name":"greet","arguments":[{"name":"who","required":true}]}]}'
      ;;
    *'"method":"prompts/get"'*)
      echo prompts/get >>"$log"
      who=$(printf '%s\n' "$line" | sed -n 's/.*"who":"\([^"]*\)".*/\1/p')
      result "{\"messages\":[{\"role\":\"user\",\"content\":{\"type\":\"text\",\"text\":\"Greet $who\"}}]}"
      ;;
    *)
      if [ -n "$id" ]; then failure -32601 'Method not found'; fi
      ;;
  esac
done

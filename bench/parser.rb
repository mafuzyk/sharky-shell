#!/usr/bin/env ruby
# frozen_string_literal: true

require_relative "../lib/sharky"

ITERATIONS = 10_000
TEMPLATES = [
  %(printf '%s\\n' "hello world" | wc -l),
  %(cd /tmp && pwd; echo "$HOME" >/dev/null),
  %(find . -name '*.rb' -type f 2>/dev/null | head -n 20),
  %(echo $(printf '%s' "$USER") ${HOME} $? $$ $!),
  %(VAR=1 command --flag value >/tmp/out 2>&1),
  %( (cd /tmp && ls -la) || echo failed ),
  %(git status --short --branch && echo done),
  %(echo $(printf '%s' "$(pwd)") | sed 's|/|_|g')
].freeze

lines = Array.new(ITERATIONS) { |index| TEMPLATES[index % TEMPLATES.length].ljust(100) }

1_000.times { |index| Sharky::Parser.parse(lines[index % lines.length]) }

started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
lines.each { |line| Sharky::Parser.parse(line) }
elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started

per_line = (elapsed * 1000) / ITERATIONS
puts format("parser benchmark: %d lines in %.3f s (%.4f ms/line)", ITERATIONS, elapsed, per_line)

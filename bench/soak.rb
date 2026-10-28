#!/usr/bin/env ruby
# frozen_string_literal: true

require "open3"
require "pty"
require "timeout"
require "tmpdir"
require "fileutils"

ROOT = File.expand_path("..", __dir__)
BIN = File.join(ROOT, "bin", "sharky")
ROUNDS = 6
PTY_ROUNDS = 3
PROMPT = /\xE2\x9D\xAF/n

CASES = {
  "if" => ["if true; then printf yes; fi", "yes", 0],
  "for" => ["for x in a b; do printf %s \"$x\"; done", "ab", 0],
  "while" => ["i=0; while [ \"$i\" -lt 2 ]; do printf %s \"$i\"; i=$((i + 1)); done", "01", 0],
  "array" => ["a=(one two); printf '%s' \"${a[1]}\"", "two", 0],
  "redirect" => ["printf hi > \"$SOAK_TMP\"; cat \"$SOAK_TMP\"", "hi", 0],
  "pipeline" => ["printf 'a\\nb\\n' | wc -l", "2\n", 0],
  "here-doc" => ["cat <<EOF\nhi\nEOF\n", "hi\n", 0],
  "trap-exit" => ["trap 'printf bye' EXIT; printf hi", "hibye", 0],
  "errexit" => ["set -e; false; printf no", "", 1]
}.freeze

def fail!(message)
  warn "soak: #{message}"
  exit 1
end

Dir.mktmpdir("sharky-soak-") do |dir|
  env = {
    "HOME" => dir,
    "PATH" => "/usr/bin:/bin",
    "LC_ALL" => "C.UTF-8",
    "TERM" => "xterm-256color",
    "SOAK_TMP" => File.join(dir, "redirect")
  }

  ROUNDS.times do |round|
    CASES.each do |name, (command, expected_out, expected_status)|
      out, err, status = Open3.capture3(env, BIN, "--no-config", "-c", command)
      fail!("#{name} round #{round}: stdout #{out.inspect} != #{expected_out.inspect}") unless out == expected_out
      fail!("#{name} round #{round}: status #{status.exitstatus} != #{expected_status}") unless status.exitstatus == expected_status
      fail!("#{name} round #{round}: stderr #{err.inspect}") unless err.empty?
    end
  end

  PTY_ROUNDS.times do |round|
    reader, writer, pid = PTY.spawn(env, BIN, "--no-config")
    reader.binmode
    writer.binmode
    output = +"".b
    begin
      Timeout.timeout(5) do
        output << reader.readpartial(4096) until output.match?(PROMPT)
      end
      writer.write("if true; then printf pty-ok; fi\r")
      Timeout.timeout(5) do
        output << reader.readpartial(4096) until output.match?(/pty-ok/)
      end
      writer.write("exit 0\r")
      _, status = Timeout.timeout(5) { Process.wait2(pid) }
      fail!("pty round #{round}: status #{status.exitstatus}") unless status.exitstatus.zero?
      fail!("pty round #{round}: missing pty-ok") unless output.include?("pty-ok")
    rescue Timeout::Error
      fail!("pty round #{round}: timed out; got #{output.inspect}")
    ensure
      Process.kill("KILL", -pid) rescue nil
      Process.kill("KILL", pid) rescue nil
      Process.wait(pid) rescue nil
      writer.close rescue nil
      reader.close rescue nil
    end
  end
end

puts "soak: #{CASES.length * ROUNDS} command runs and #{PTY_ROUNDS} PTY sessions passed"

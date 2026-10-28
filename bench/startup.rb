#!/usr/bin/env ruby
# frozen_string_literal: true

require "rbconfig"

ROOT = File.expand_path("..", __dir__)
EXECUTABLE = File.join(ROOT, "bin/sharky")
RUNS = 20
WARMUP = 3

def measure(command)
  started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  pid = Process.spawn(*command, out: File::NULL, err: File::NULL)
  _, status = Process.waitpid2(pid)
  raise "benchmark command failed: #{command.join(" ")}" unless status.success?

  (Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000
end

def percentile(samples, fraction)
  sorted = samples.sort
  sorted[(fraction * (sorted.length - 1)).round]
end

scenarios = [
  ["bin/sharky --version", [EXECUTABLE, "--version"]],
  ["bin/sharky -c true", [EXECUTABLE, "-c", "true"]],
  ["bin/sharky --no-config -c true", [EXECUTABLE, "--no-config", "-c", "true"]],
  ["bin/sharky --help", [EXECUTABLE, "--help"]],
  ["bin/sharky --no-config --json -c true", [EXECUTABLE, "--no-config", "--json", "-c", "true"]],
  ["bin/sharky --no-config state --json", [EXECUTABLE, "--no-config", "state", "--json"]],
  ["ruby --disable-gems -e 0", [RbConfig.ruby, "--disable-gems", "-e", "0"]],
  ["require sharky (--disable-gems)", [RbConfig.ruby, "--disable-gems", "-I#{File.join(ROOT, 'lib')}", "-rsharky", "-e", "0"]]
]

puts "startup benchmark (n=#{RUNS} after #{WARMUP} warmups)"
puts format("%-34s %10s %10s", "command", "median", "p95")

scenarios.each do |label, command|
  WARMUP.times { measure(command) }
  samples = Array.new(RUNS) { measure(command) }
  median = percentile(samples, 0.5)
  p95 = percentile(samples, 0.95)
  puts format("%-34s %7.1f ms %7.1f ms", label, median, p95)
end

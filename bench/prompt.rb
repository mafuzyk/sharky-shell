#!/usr/bin/env ruby
# frozen_string_literal: true

require_relative "../lib/sharky"
require "rbconfig"

ITERATIONS = 1000
WARMUP = 50

context = Sharky::Prompt::Context.new(
  cwd: Dir.pwd,
  home: ENV["HOME"],
  status: 0,
  duration_ms: 10,
  git_branch: "v1",
  git_dirty: false,
  os: "linux",
  arch: RbConfig::CONFIG["host_cpu"],
  ssh: false,
  user: "user",
  host: "host",
  jobs_count: 0,
  time: Time.now
)
theme = Sharky::Theme.new

def percentile(samples, fraction)
  sorted = samples.sort
  sorted[(fraction * (sorted.length - 1)).round]
end

WARMUP.times { Sharky::Prompt.render(context, theme: theme) }
samples = Array.new(ITERATIONS) do
  started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  Sharky::Prompt.render(context, theme: theme)
  (Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000
end

puts "prompt benchmark (n=#{ITERATIONS}, warmup=#{WARMUP})"
puts format("median %.3f ms  p95 %.3f ms", percentile(samples, 0.5), percentile(samples, 0.95))

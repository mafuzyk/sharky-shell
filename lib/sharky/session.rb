# frozen_string_literal: true

require "stringio"

module Sharky
  class Session
    attr_reader :shell, :runtime

    def initialize(env: ENV.to_h, cwd: Dir.pwd, stdin: nil)
      @runtime = Runtime.new
      @shell = Shell.new(
        env: env,
        cwd: cwd,
        stdin: stdin || StringIO.new,
        stdout: StringIO.new,
        stderr: StringIO.new,
        runtime: @runtime
      )
      @runtime.shell = @shell
    end

    def env
      @shell.env
    end

    def cwd
      @shell.cwd
    end

    def last_status
      @shell.last_status
    end

    def run(source)
      before = @shell.env.dup
      stdout = StringIO.new
      stderr = StringIO.new
      @shell.stdout = stdout
      @shell.stderr = stderr

      started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      status = @shell.run(source.to_s)
      duration_ms = ((Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000).round

      Result.new(
        status: status,
        stdout: stdout.string,
        stderr: stderr.string,
        duration_ms: duration_ms,
        cwd: @shell.cwd,
        env_delta: env_delta(before),
        schema_version: Result::SCHEMA_VERSION
      )
    end

    private

    def env_delta(before)
      delta = {}
      @shell.env.each do |name, value|
        delta[name] = value unless before[name] == value
      end
      before.each_key do |name|
        delta[name] = nil unless @shell.env.key?(name)
      end
      delta
    end
  end
end

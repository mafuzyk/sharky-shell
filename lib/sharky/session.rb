# frozen_string_literal: true

require "stringio"
require_relative "result"
require_relative "runtime"
require_relative "shell"

module Sharky
  class Session
    CURRENT_KEY = :__sharky_current_session__

    class << self
      def current
        Thread.current[CURRENT_KEY]
      end

      def current=(session)
        unless session.nil? || session.is_a?(self)
          raise TypeError, "current session must be a Sharky::Session"
        end

        Thread.current[CURRENT_KEY] = session
      end

      def from_runtime(runtime)
        runtime.session || new(runtime: runtime)
      end
    end

    attr_reader :runtime

    def initialize(env: ENV.to_h, cwd: Dir.pwd, stdin: nil, runtime: nil)
      if runtime
        @runtime = runtime
        @shell = nil
      else
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
      @runtime.session = self
    end

    def shell
      @shell || @runtime.shell
    end

    def env
      shell!.env
    end

    def cwd
      shell!.cwd
    end

    def last_status
      shell!.last_status
    end

    def run(source)
      current_shell = shell!
      before = current_shell.env.dup
      previous_stdout = current_shell.stdout
      previous_stderr = current_shell.stderr
      stdout = StringIO.new
      stderr = StringIO.new
      current_shell.stdout = stdout
      current_shell.stderr = stderr

      begin
        started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
        status = current_shell.run(source.to_s, compound: true)
        duration_ms = ((Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000).round

        Result.new(
          status: status,
          stdout: stdout.string,
          stderr: stderr.string,
          duration_ms: duration_ms,
          cwd: current_shell.cwd,
          env_delta: env_delta(current_shell, before),
          schema_version: Result::SCHEMA_VERSION
        )
      ensure
        current_shell.stdout = previous_stdout
        current_shell.stderr = previous_stderr
      end
    end

    private

    def shell!
      shell || raise(RuntimeError, "Sharky shell is not initialized")
    end

    def env_delta(shell, before)
      delta = {}
      shell.env.each do |name, value|
        delta[name] = value unless before[name] == value
      end
      before.each_key do |name|
        delta[name] = nil unless shell.env.key?(name)
      end
      delta
    end
  end
end

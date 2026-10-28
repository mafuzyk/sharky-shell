# frozen_string_literal: true

module Sharky
  class Result < Data.define(:status, :stdout, :stderr, :duration_ms, :cwd, :env_delta, :schema_version)
    SCHEMA_VERSION = 1

    def initialize(status:, stdout:, stderr:, duration_ms:, cwd:, env_delta:, schema_version: SCHEMA_VERSION)
      super(
        status: status,
        stdout: stdout,
        stderr: stderr,
        duration_ms: duration_ms,
        cwd: cwd,
        env_delta: env_delta.to_h { |name, value| [name.dup.freeze, value&.dup&.freeze] }.freeze,
        schema_version: schema_version
      )
    end
  end
end

# frozen_string_literal: true

require "open3"

module Sharky
  module Starship
    DEFAULT_TIMEOUT = 0.2

    class Error < StandardError; end

    class << self
      def available?(command: "starship")
        name = command.to_s
        return false if name.empty?

        if name.include?(File::SEPARATOR)
          executable?(name)
        else
          ENV["PATH"].to_s.split(File::PATH_SEPARATOR).any? do |directory|
            next false if directory.empty?

            executable?(File.join(directory, name))
          end
        end
      rescue ArgumentError
        false
      end

      def prompt(context, command: "starship", config: nil, timeout: DEFAULT_TIMEOUT)
        env = { "STARSHIP_SHELL" => "sharky" }
        env["STARSHIP_CONFIG"] = config.to_s if config
        args = [
          "prompt",
          "--status", context.status.to_i.to_s,
          "--cmd-duration", context.duration_ms.to_i.to_s,
          "--jobs", context.jobs_count.to_i.to_s
        ]
        output, error, status = capture(command, args, env, context.cwd, timeout)

        unless status.success?
          message = "starship prompt failed with exit status #{status.exitstatus || status.termsig}"
          message += ": #{error.strip}" unless error.strip.empty?
          raise Error, message
        end

        output.chomp
      rescue SystemCallError => e
        raise Error, "starship prompt failed: #{e.message}"
      end

      private

      def executable?(path)
        File.file?(path) && File.executable?(path)
      end

      def capture(command, args, env, cwd, timeout)
        output = +""
        error = +""
        status = nil
        cwd = Dir.pwd if cwd.nil? || cwd.to_s.empty?

        Open3.popen3(env, command, *args, chdir: cwd, pgroup: true) do |stdin, stdout, stderr, wait_thread|
          stdin.close
          out_reader = Thread.new { stdout.read }
          err_reader = Thread.new { stderr.read }
          begin
            unless wait_thread.join(Float(timeout))
              kill_group(wait_thread.pid)
              wait_thread.join
              raise Error, "starship prompt timed out after #{timeout}s"
            end
            status = wait_thread.value
          ensure
            output = out_reader.value
            error = err_reader.value
          end
        end

        [output, error, status]
      end

      def kill_group(pid)
        Process.kill("KILL", -pid)
      rescue Errno::ESRCH, Errno::EPERM
        nil
      end
    end
  end
end

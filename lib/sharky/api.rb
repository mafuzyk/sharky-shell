# frozen_string_literal: true

require_relative "runtime"
require_relative "session"

module Sharky
  class << self
    def session
      Session.current
    end

    def session=(session)
      Session.current = session
    end

    def runtime
      session&.runtime
    end

    def runtime=(runtime)
      self.session = runtime && Session.from_runtime(runtime)
    end

    def prompt
      runtime&.prompt
    end

    def prompt=(callable)
      runtime!.prompt = callable
    end

    def prompt_preset(name)
      require_relative "prompt_presets"
      runtime!.prompt = PromptPresets.fetch(name)
    end

    def starship(config: nil, command: "starship", timeout: 0.2)
      runtime!.starship = { config: config, command: command, timeout: timeout }
    end

    def theme(name = nil)
      return runtime&.theme if name.nil?
      return self.theme = name if name.is_a?(Hash)

      load_theme(name)
    end

    def theme=(styles)
      current = runtime!
      current.theme = current.theme.merge(styles)
    end

    def alias(name, value)
      value = value.to_s
      raise ArgumentError, "control operator in alias value: #{value}" if Runtime.control_operator?(value)

      runtime!.aliases[name.to_s] = value
    end

    def abbr(name, value)
      runtime!.abbreviations[name.to_s] = value.to_s
    end

    def bind(key, action)
      runtime!.bindings[key.to_s] = action
    end

    def command(name, &block)
      raise ArgumentError, "command requires a block" unless block

      runtime!.commands[name.to_s] = block
    end

    def complete(name, &block)
      raise ArgumentError, "complete requires a block" unless block

      runtime!.completions[name.to_s] = block
    end

    def sh(source)
      active_shell.run(source.to_s)
    end

    def capture(source)
      shell = active_shell
      require "tempfile"

      Tempfile.create("sharky-capture") do |file|
        previous = shell.stdout
        begin
          shell.stdout = file
          shell.run(source.to_s)
          file.flush
          file.rewind
          file.read
        ensure
          shell.stdout = previous
        end
      end
    end

    def cwd
      active_shell.cwd
    end

    def env
      active_shell.env
    end

    def last_status
      active_shell.last_status
    end

    def pid
      Process.pid
    end

    private

    def load_theme(name)
      runtime = runtime!
      theme_name = name.to_s
      raise ArgumentError, "invalid theme name: #{theme_name}" unless theme_name.match?(/\A[A-Za-z0-9_-]+\z/)

      directory = File.join(runtime.config_dir.to_s, "themes")
      toml_path = File.join(directory, "#{theme_name}.toml")
      ruby_path = File.join(directory, "#{theme_name}.rb")

      if File.file?(toml_path)
        require_relative "toml"
        require_relative "prompt_layout"
        data = Toml.parse(File.read(toml_path), path: toml_path)
        runtime.theme_layout = data["prompt"].is_a?(Hash) ? PromptLayout.from_toml(data) : nil
        styles = data["styles"] if data["styles"].is_a?(Hash)
        styles ||= data
        styles = styles.to_h.transform_keys(&:to_sym)
        runtime.theme = runtime.theme.merge(styles)
        return runtime.theme
      end

      raise ArgumentError, "theme not found: #{theme_name}" unless File.file?(ruby_path)

      runtime.theme_layout = nil
      load(ruby_path)
      runtime.theme
    end

    def runtime!
      session&.runtime || raise(RuntimeError, "Sharky runtime is not initialized")
    end

    def active_shell
      session&.shell || raise(RuntimeError, "Sharky shell is not initialized")
    end
  end
end

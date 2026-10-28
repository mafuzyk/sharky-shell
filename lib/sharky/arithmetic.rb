# frozen_string_literal: true

module Sharky
  class Arithmetic
    Error = Class.new(StandardError)
    TOKEN = /\G\s*(\d+|[A-Za-z_][A-Za-z0-9_]*|\$[A-Za-z_][A-Za-z0-9_]*|==|!=|<=|>=|&&|\|\||[()+\-*\/%!<>])/

    def self.evaluate(source, env: {})
      new(source, env: env).evaluate
    end

    def initialize(source, env: {})
      @source = source.to_s
      @tokens = tokenize
      @pos = 0
      @env = env
    end

    def evaluate
      value = parse_or
      error("unexpected token #{current.inspect}") unless current.nil?
      value
    end

    private

    def tokenize
      tokens = []
      index = 0
      while index < @source.length
        match = TOKEN.match(@source, index)
        break if match.nil? || match.begin(0) != index
        break if match[1].nil?

        tokens << match[1]
        index = match.end(0)
      end
      error("unexpected token near #{@source[index..].inspect}") unless @source[index..].to_s.match?(/\A\s*\z/)
      tokens
    end

    def current
      @tokens[@pos]
    end

    def advance
      token = current
      @pos += 1
      token
    end

    def accept(token)
      return false unless current == token

      advance
      true
    end

    def parse_or
      value = parse_and
      while accept("||")
        right = parse_and
        value = truthy?(value) || truthy?(right) ? 1 : 0
      end
      value
    end

    def parse_and
      value = parse_comparison
      while accept("&&")
        right = parse_comparison
        value = truthy?(value) && truthy?(right) ? 1 : 0
      end
      value
    end

    def parse_comparison
      value = parse_add
      while (operator = current) && %w[== != < <= > >=].include?(operator)
        advance
        right = parse_add
        value = compare(value, operator, right)
      end
      value
    end

    def parse_add
      value = parse_multiply
      while (token = current) && %w[+ -].include?(token)
        advance
        right = parse_multiply
        value = token == "+" ? value + right : value - right
      end
      value
    end

    def parse_multiply
      value = parse_unary
      while (token = current) && %w[* / %].include?(token)
        advance
        right = parse_unary
        case token
        when "*" then value *= right
        when "/"
          error("division by zero") if right.zero?
          value /= right
        when "%"
          error("division by zero") if right.zero?
          value %= right
        end
      end
      value
    end

    def parse_unary
      if accept("+")
        parse_unary
      elsif accept("-")
        -parse_unary
      elsif accept("!")
        truthy?(parse_unary) ? 0 : 1
      else
        parse_primary
      end
    end

    def parse_primary
      token = advance
      error("unexpected end of expression") if token.nil?

      if token == "("
        value = parse_or
        error("missing ')'") unless accept(")")
        value
      elsif token.match?(/\A\d+\z/)
        token.to_i
      elsif token.match?(/\A\$?[A-Za-z_][A-Za-z0-9_]*\z/)
        Integer(@env[token.delete_prefix("$")].to_s, 10)
      else
        error("unexpected token #{token.inspect}")
      end
    rescue ArgumentError
      0
    end

    def compare(left, operator, right)
      case operator
      when "==" then left == right ? 1 : 0
      when "!=" then left != right ? 1 : 0
      when "<" then left < right ? 1 : 0
      when "<=" then left <= right ? 1 : 0
      when ">" then left > right ? 1 : 0
      when ">=" then left >= right ? 1 : 0
      end
    end

    def truthy?(value)
      !value.zero?
    end

    def error(message)
      raise Error, message
    end
  end
end

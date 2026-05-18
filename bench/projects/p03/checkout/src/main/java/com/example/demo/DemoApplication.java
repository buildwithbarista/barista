package com.example.demo;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RestController;

/**
 * Minimal Spring Boot starter-web entry point.
 *
 * <p>This class exists purely so the {@code spring-boot-maven-plugin}
 * has a {@code @SpringBootApplication}-annotated main class to anchor
 * the build on. The HTTP endpoint is never hit — the corpus exists to
 * exercise dependency resolution traffic, not runtime behavior.
 */
@SpringBootApplication
@RestController
public class DemoApplication {

    public static void main(String[] args) {
        SpringApplication.run(DemoApplication.class, args);
    }

    @GetMapping("/")
    public String index() {
        return "hello";
    }
}

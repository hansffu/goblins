import java.net.Socket;
import java.time.Duration;
import org.testcontainers.DockerClientFactory;
import org.testcontainers.containers.GenericContainer;

class TestcontainersSmoke {
    public static void main(String[] args) throws Exception {
        var postgres = new GenericContainer<>("postgres:16-alpine")
            .withEnv("POSTGRES_PASSWORD", "goblins-test")
            .withLabel("goblins.testcontainers-smoke", "true")
            .withExposedPorts(5432)
            .withStartupTimeout(Duration.ofSeconds(90));
        postgres.start();
        try (var socket = new Socket(postgres.getHost(), postgres.getMappedPort(5432))) {
            if (!socket.isConnected()) throw new AssertionError("published port unreachable");
        }
        var sql = postgres.execInContainer("psql", "-U", "postgres", "-Atc", "SELECT 42");
        if (sql.getExitCode() != 0 || !sql.getStdout().trim().equals("42")) {
            throw new AssertionError("Postgres query failed: " + sql);
        }
        var docker = DockerClientFactory.instance().client();
        var ryuk = docker.inspectContainerCmd("testcontainers-ryuk-" + DockerClientFactory.SESSION_ID).exec();
        if (Boolean.TRUE.equals(ryuk.getHostConfig().getPrivileged())) {
            throw new AssertionError("Ryuk must run without privileged mode");
        }
        System.out.println("PASS: Testcontainers Postgres, published port, SQL and unprivileged Ryuk");
        // No close() and no shutdown hook: the caller verifies Ryuk reaps it.
        Runtime.getRuntime().halt(0);
    }
}
